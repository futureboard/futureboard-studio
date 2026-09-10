//! `FutureboardPluginHostX64.exe` — the separated VST3 plugin/editor host
//! process (IPC *server*).
//!
//! VST3 editor hosting follows public.sdk/samples/vst-hosting/editorhost
//! lifecycle: the host owns the COM STA thread and the editor message pump, and
//! drives `createView`/`attached`/`onSize`/`removed` via the proven C++ backend
//! (`SpherePluginHost::native_editor`). What is new here is *where* it runs:
//! out-of-process, so a crashing plugin editor cannot take down the GPUI main
//! app.
//!
//! In `main_owned_window` mode (Slice 1 default) the **visible editor window is
//! owned by the main app** — this process only receives an HWND over IPC and
//! attaches the VST3 view to it. The host therefore never creates a top-level
//! editor window; it only pumps messages so the attached `IPlugView` repaints.
//!
//! Protocol: [`HostCommand`] frames arrive on **stdin**, [`HostEvent`] frames
//! are written to **stdout**, human logs go to **stderr** behind
//! `FUTUREBOARD_PLUGIN_VIEW_DEBUG`. See [`SpherePluginHost::ipc`].

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io::{self, BufReader};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use builtin_dsp_core::{Instrument, StereoEffect};
use SpherePluginHost::au_host::{AuHostProcessor, AuTransport};
use SpherePluginHost::audio_bridge::{
    bridge_kick_event_name, BridgeKickEvent, SharedAudioRegion, SharedMidiEvent, AUDIO_BUF_LEN,
    MAX_BLOCK_FRAMES, MAX_CHANNELS,
};
use SpherePluginHost::ipc::{self, EditorChromeCommand, HostCommand, HostEvent, PROTOCOL_VERSION};
use SpherePluginHost::native_editor::{self, EmbedRegion};
use SpherePluginHost::plugin_host_preview::{
    try_start_preview_output, BridgeAudioShared, PluginHostPreviewEngine, SharedPluginHostPreview,
};
use SpherePluginHost::spectrum::{SpectrumAnalyzer, SPECTRUM_BINS};

fn debug_enabled() -> bool {
    // Cached: this is checked on the audio producer's per-block path, which
    // must never take the std env lock.
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some())
}

/// Whether the **temporary** separate-CPAL preview output is allowed.
///
/// Stage 1: this is OFF by default. Plugin DSP output is meant to flow into the
/// main DAW engine (mixer / master / meters), not a second device stream, so we
/// must not "fake success" with a private CPAL stream. Until the shared-memory
/// mix path (Stage 3) lands, preview MIDI is still queued to the VSTi but no
/// audio device is opened and the host logs `dsp_output=pending`. Set
/// `FUTUREBOARD_PLUGIN_HOST_CPAL_PREVIEW=1` to opt into the legacy audition
/// stream for manual testing only.
fn debug_audio_out_enabled() -> bool {
    // Cached: checked on the audio producer's per-block path (see above).
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("FUTUREBOARD_PLUGIN_HOST_DEBUG_AUDIO_OUT")
            .or_else(|_| std::env::var("FUTUREBOARD_PLUGIN_HOST_CPAL_PREVIEW"))
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                matches!(v.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    })
}

fn log_host_audio_mode() {
    let debug = debug_audio_out_enabled();
    eprintln!("[plugin-host-audio] debug_audio_out={debug}");
    eprintln!(
        "[plugin-host-audio] device_stream={}",
        if debug { "debug_only" } else { "disabled" }
    );
    eprintln!("[plugin-host-dsp] output_to=shared_audio_bridge");
}

macro_rules! hlog {
    ($($arg:tt)*) => {{
        if debug_enabled() {
            eprintln!($($arg)*);
        }
    }};
}

static MAIN_DAW_HWND: AtomicU64 = AtomicU64::new(0);

fn store_main_hwnd(hwnd: Option<u64>) {
    let value = hwnd.unwrap_or(0);
    MAIN_DAW_HWND.store(value, Ordering::SeqCst);
    if value != 0 {
        eprintln!("[PluginHost] main_hwnd received hwnd=0x{value:x}");
    }
}

#[allow(dead_code)]
fn main_hwnd() -> u64 {
    MAIN_DAW_HWND.load(Ordering::SeqCst)
}

fn parse_parent_pid() -> Option<u32> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--parent-pid" {
            return args.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

/// Widen the default stack new Rust threads get in this process, before any
/// plugin is loaded or its threads spawned.
///
/// A VST3 editor opens on the GTK thread (`editor_linux.cpp`'s
/// `gtk_thread_main`), which calls into the plug-in's `IPlugView::attached()`.
/// A Rust-built plug-in whose editor spawns its own thread on Rust's default
/// (2 MiB, `std::thread::spawn` with no explicit `stack_size`) can overflow it
/// there: Mesa's GLX/DRI path — `glXChooseFBConfig` walking into driver setup
/// that parses `drirc` via libexpat — is far deeper on Linux than the
/// equivalent WGL call on Windows, which never re-enters into a Rust-thread
/// stack budget like this at all. Confirmed by reproduction: EQUZX.vst3
/// (nih_plug + baseview + egui_glow) segfaults inside `glXChooseFBConfig`
/// with the fault address landing on the thread's own stack pointer — the
/// signature of exhausting it, not a data race — and setting `RUST_MIN_STACK`
/// before that thread exists prevents the crash.
///
/// `RUST_MIN_STACK` is read from the process environment (via libc getenv),
/// which is shared by every `dlopen`'d plug-in in this process regardless of
/// which copy of the Rust runtime it statically links, so setting it here
/// covers any Rust-built plug-in that relies on the default rather than just
/// this one. Only raises it — an explicit caller override always wins.
#[cfg(target_os = "linux")]
fn raise_default_thread_stack_for_plugin_editors() {
    const MIN_STACK_BYTES: &str = "16777216"; // 16 MiB, well past the ~2 MiB default.
    if std::env::var_os("RUST_MIN_STACK").is_none() {
        // SAFETY: called first thing in `main`, before any other thread
        // (including the plug-in threads this exists to protect) is spawned.
        unsafe {
            std::env::set_var("RUST_MIN_STACK", MIN_STACK_BYTES);
        }
        eprintln!("[plugin-host] RUST_MIN_STACK={MIN_STACK_BYTES} (linux GLX/DRI stack headroom)");
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    raise_default_thread_stack_for_plugin_editors();

    let selftest = std::env::args().any(|a| a == "--selftest");
    let parent_pid = parse_parent_pid();

    let _log_path = SpherePluginHost::plugin_host_logging::init_host_logging();
    SpherePluginHost::plugin_host_logging::log_startup_environment();

    // Match the DAW's explicit AppUserModelID so plugin-editor windows this
    // process creates never group as a separate taskbar app (spec: process
    // identity). Owned WS_EX_TOOLWINDOW popups already stay off the taskbar /
    // Alt-Tab; this is belt-and-braces against accidental app-visibility.
    SpherePluginHost::plugin_host_lifecycle::set_futureboard_app_user_model_id();
    // ... and take the one window this process can still show off the taskbar.
    // A debug build is a console subsystem app, and that console is a blank
    // window sitting among the DAW's own.
    if !selftest {
        SpherePluginHost::plugin_host_lifecycle::hide_own_console_window();
    }

    platform::com_init();
    platform::ensure_dpi_awareness();
    let pid = std::process::id();
    let thread_id = platform::current_thread_id();
    hlog!("[PluginHostEditor] start pid={pid} thread_id={thread_id} selftest={selftest}");
    if let Some(parent_pid) = parent_pid {
        eprintln!("[plugin-host] parent_pid={parent_pid}");
        eprintln!(
            "[PluginHostProcess] pid={pid} parent_pid={parent_pid} expected_parent=FutureboardStudio"
        );
    }

    // Confirm the main app stripped its renderer-only environment before
    // spawning us (spec Part 1). The host must run with a clean native
    // environment so plugin GPU/WebView/DirectComposition UI can paint.
    log_renderer_env();
    log_runtime_policy();

    if selftest {
        let code = run_selftest();
        platform::com_uninit();
        std::process::exit(code);
    }

    let mut out = io::stdout();
    let _ = ipc::write_frame(
        &mut out,
        &HostEvent::Ready {
            protocol_version: PROTOCOL_VERSION,
            pid,
        },
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    if let Some(parent_pid) = parent_pid {
        let shutdown_flag = shutdown.clone();
        std::thread::Builder::new()
            .name("plugin-host-parent-watch".into())
            .spawn(move || parent_watchdog(parent_pid, shutdown_flag))
            .expect("spawn parent watchdog");
    }

    run_ipc_loop(out, shutdown);
    platform::com_uninit();
}

/// Announce the runtime-ownership policy this host enforces. The external
/// bridge is always authoritative: there is no in-process VST3 runtime and no
/// legacy editor unless `FUTUREBOARD_LEGACY_PLUGIN_EDITOR` is explicitly set.
fn log_runtime_policy() {
    let legacy_enabled = std::env::var_os("FUTUREBOARD_LEGACY_PLUGIN_EDITOR").is_some();
    eprintln!("[plugin-runtime-policy] external_bridge_forced=true");
    eprintln!("[plugin-runtime-policy] legacy_editor_enabled={legacy_enabled}");
    eprintln!("[plugin-runtime-policy] in_process_runtime_allowed=false");
    if legacy_enabled {
        eprintln!(
            "[plugin-runtime-policy] WARNING legacy plugin editor/runtime enabled by FUTUREBOARD_LEGACY_PLUGIN_EDITOR=1"
        );
    }
}

/// Report whether the main-app-only renderer environment leaked into this
/// process. After the spawn-side `sanitize_child_env` fix these should all read
/// `<unset>`; a `set` here means an env var is still being inherited.
fn log_renderer_env() {
    let role = std::env::var("FUTUREBOARD_PROCESS_ROLE").unwrap_or_else(|_| "<unset>".into());
    eprintln!("[plugin-host] FUTUREBOARD_PROCESS_ROLE={role}");
    let dcomp = if std::env::var_os("GPUI_DISABLE_DIRECT_COMPOSITION").is_some() {
        "set"
    } else {
        "<unset>"
    };
    eprintln!("[plugin-host] GPUI_DISABLE_DIRECT_COMPOSITION={dcomp}");
    let leaked = std::env::vars().any(|(k, _)| {
        k.starts_with("GPUI_")
            || k.starts_with("WGPU_")
            || k == "DXGI_PRESENT_ALLOW_TEARING"
            || k == "LIBGL_ALWAYS_SOFTWARE"
    });
    if leaked {
        eprintln!("[plugin-host-env] sanitized=false");
    } else {
        eprintln!("[plugin-host-env] sanitized=true");
    }
}

/// Editor states keyed by `plugin_instance_id` — the in-process
/// `PluginEditorRegistry` role, living inside the host process.
#[derive(Debug, Clone)]
struct EditorState {
    plugin_instance_id: String,
    host_hwnd: u64,
    owner_hwnd: u64,
    display_title: String,
    state: &'static str,
}

type Registry = HashMap<String, EditorState>;

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct LoadedPlugin {
    plugin_path: String,
    class_id: String,
    name: String,
    sample_rate: u32,
    max_block_size: u32,
    processing_ready: bool,
}

type LoadedRegistry = HashMap<String, LoadedPlugin>;

/// The DSP core behind a built-in insert, one variant per bridge-enabled
/// built-in. The variants deliberately do not share a trait: they differ in
/// what they *have* (block-boundary hand-off, meters, latency), and a
/// lowest-common-denominator trait would force every core to pretend it has all
/// three. Keep in sync with `SpherePluginHost::builtin::AUDIO_BRIDGE_STEMS`.
///
/// The variants differ in size by tens of kilobytes, which costs nothing here:
/// exactly one is built per insert, in place inside an `Arc`'d
/// `BuiltinHostProcessor`, and it is never moved or collected. Boxing the large
/// variants to even out the enum would only add a pointer hop to every
/// `process_block` on the audio producer thread.
#[allow(clippy::large_enum_variant)]
enum BuiltinDsp {
    Rodhareist(rodharerist::Dsp),
    Equz8(equz8::Dsp),
    Verbspace(verbspace::Dsp),
    Echospace(echospace::Dsp),
    Fa2a(fa2a::Dsp),
    Fa76(fa76::Dsp),
    BurnLimit(burnlimit::Dsp),
    Clipper67(clipper67::Dsp),
    Transient(transient::Dsp),
    WrapSynth(wrapsynth::Dsp),
    Zcomp(zcomp::Dsp),
    MixStation(mixstation::Dsp),
}

/// A built-in processor is created on the IPC thread and then owned exclusively
/// by the audio producer thread. The map only publishes/removes `Arc` handles;
/// no other thread accesses the DSP inside the cell. The one sanctioned
/// exceptions are `nam_loader` and `ir_loader`: `Arc`'d pairs of wait-free
/// hand-off cells the IPC thread uses to submit prepared NAM captures and
/// impulse responses (adopted by the producer at `begin_block`) — neither ever
/// touches the `Dsp` itself.
struct BuiltinHostProcessor {
    dsp: UnsafeCell<BuiltinDsp>,
    /// Analyser over the insert's **pre-DSP** input, for the editor's spectrum
    /// overlay. Pre rather than post on purpose: the editor draws the response
    /// curve on top of it, so an input spectrum shows cause and effect in one
    /// picture, where a post-DSP one would double-count the same processing.
    /// Producer-thread owned, same exclusivity contract as `dsp`.
    spectrum: UnsafeCell<SpectrumAnalyzer>,
    /// Control-side NAM capture loader (safe from the IPC thread). `None` for a
    /// built-in with no capture stage.
    nam_loader: Option<rodharerist::NamLoader>,
    /// Control-side impulse-response loader (safe from the IPC thread). `None`
    /// for a built-in with no cabinet stage.
    ir_loader: Option<rodharerist::IrLoader>,
    /// Engine sample rate the DSP was built at — needed to validate a `.nam`
    /// capture's declared rate on the IPC thread.
    sample_rate: f32,
}

// SAFETY: `process_block` is called only by the single audio producer thread.
// IPC/UI threads only clone or remove the surrounding Arc map entry, or use
// the `nam_loader`/`ir_loader` hand-off cells, thread-safe by construction.
unsafe impl Send for BuiltinHostProcessor {}
unsafe impl Sync for BuiltinHostProcessor {}

impl BuiltinHostProcessor {
    /// Build the DSP for a built-in catalog stem, or `None` when the host has
    /// no core for it. The caller turns `None` into a load failure rather than
    /// publishing a silent instance.
    fn new(stem: &str, sample_rate: u32, state_json: Option<&str>) -> Option<Self> {
        match stem {
            "rodharerist" => Some(Self::rodhareist(sample_rate, state_json)),
            "equz8" => Some(Self::equz8(sample_rate, state_json)),
            "verbspace" => Some(Self::verbspace(sample_rate, state_json)),
            "echospace" => Some(Self::echospace(sample_rate, state_json)),
            "fa2a" => Some(Self::fa2a(sample_rate, state_json)),
            "fa76" => Some(Self::fa76(sample_rate, state_json)),
            "burnlimit" => Some(Self::burnlimit(sample_rate, state_json)),
            "clipper67" => Some(Self::clipper67(sample_rate, state_json)),
            "transient" => Some(Self::transient(sample_rate, state_json)),
            "wrapsynth" => Some(Self::wrapsynth(sample_rate, state_json)),
            "zcomp" => Some(Self::zcomp(sample_rate, state_json)),
            "mixstation" => Some(Self::mixstation(sample_rate, state_json)),
            _ => None,
        }
    }

    fn rodhareist(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = rodharerist::Dsp::new(sr);
        // Apply persisted project state here, before the processor is
        // published to the audio producer — the only window where touching
        // the DSP from this (IPC) thread is race-free by construction.
        if let Some(json) = state_json {
            match rodharerist::RodhareistState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state schema_version={}",
                        state.schema_version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        let nam_loader = Some(dsp.nam_loader());
        let ir_loader = Some(dsp.ir_loader());
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Rodhareist(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            nam_loader,
            ir_loader,
            sample_rate: sr,
        }
    }

    fn equz8(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = equz8::Dsp::new(sr);
        // Same pre-publish window as above: the IPC thread still owns the DSP.
        if let Some(json) = state_json {
            match equz8::ipc::Equz8State::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Equz8(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            // An EQ has no capture or cabinet stage to hand off into.
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn fa2a(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = fa2a::Dsp::new(sr);
        // Same pre-publish window as above: the IPC thread still owns the DSP.
        if let Some(json) = state_json {
            match fa2a::ipc::Fa2aState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Fa2a(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            // A compressor has no capture or cabinet stage to hand off into.
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn fa76(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = fa76::Dsp::new(sr);
        // Same pre-publish window as above: the IPC thread still owns the DSP.
        if let Some(json) = state_json {
            match fa76::ipc::Fa76State::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Fa76(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            // A compressor has no capture or cabinet stage to hand off into.
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn burnlimit(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = burnlimit::Dsp::new(sr);
        if let Some(json) = state_json {
            match burnlimit::ipc::BurnLimitState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::BurnLimit(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn clipper67(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = clipper67::Dsp::new(sr);
        if let Some(json) = state_json {
            match clipper67::ipc::Clipper67State::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Clipper67(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn transient(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = transient::Dsp::new(sr);
        if let Some(json) = state_json {
            match transient::ipc::TransientState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Transient(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn echospace(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = echospace::Dsp::new(sr);
        // Same pre-publish window as above: the IPC thread still owns the DSP.
        if let Some(json) = state_json {
            match echospace::ipc::EchospaceState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Echospace(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            // A delay has no capture or cabinet stage to hand off into.
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn verbspace(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = verbspace::Dsp::new(sr);
        // Same pre-publish window as above: the IPC thread still owns the DSP.
        if let Some(json) = state_json {
            match verbspace::ipc::VerbspaceState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Verbspace(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            // A reverb has no capture or cabinet stage to hand off into.
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn wrapsynth(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = wrapsynth::Dsp::new(sr);
        if let Some(json) = state_json {
            match wrapsynth::ipc::WrapSynthState::from_json(json) {
                Ok(state) => dsp.set_params(state.params),
                Err(error) => {
                    eprintln!("[plugin-host-builtin] WrapSynth state rejected: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::WrapSynth(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn zcomp(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = zcomp::Dsp::new(sr);
        if let Some(json) = state_json {
            match zcomp::ipc::ZcompState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::Zcomp(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn mixstation(sample_rate: u32, state_json: Option<&str>) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut dsp = mixstation::Dsp::new(sr);
        if let Some(json) = state_json {
            match mixstation::ipc::MixStationState::from_json(json) {
                Ok(state) => {
                    dsp.set_params(state.params);
                    eprintln!(
                        "[plugin-host-builtin] restored state version={}",
                        state.version
                    );
                }
                Err(error) => {
                    eprintln!("[plugin-host-builtin] state blob rejected, using defaults: {error}");
                }
            }
        }
        // State restore happens before publication to the producer. Start at
        // the restored trims rather than audibly ramping from the defaults.
        dsp.reset();
        Self {
            dsp: UnsafeCell::new(BuiltinDsp::MixStation(dsp)),
            spectrum: UnsafeCell::new(SpectrumAnalyzer::new(sr)),
            nam_loader: None,
            ir_loader: None,
            sample_rate: sr,
        }
    }

    fn process_block(&self, in_l: &[f32], in_r: &[f32], interleaved: &mut [f32], frames: usize) {
        // SAFETY: the dedicated producer thread is the sole DSP accessor.
        match unsafe { &mut *self.dsp.get() } {
            BuiltinDsp::Rodhareist(dsp) => {
                // Block boundary: adopt any pending NAM/IR swap (never mid-block).
                dsp.begin_block();
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Equz8(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Verbspace(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Echospace(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Fa2a(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Fa76(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::BurnLimit(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Clipper67(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Transient(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::WrapSynth(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo();
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::Zcomp(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
            BuiltinDsp::MixStation(dsp) => {
                for i in 0..frames {
                    let (l, r) = dsp.process_stereo(in_l[i], in_r[i]);
                    interleaved[i * 2] = l;
                    interleaved[i * 2 + 1] = r;
                }
            }
        }
    }

    /// Hand the block's transport tempo to the cores that derive a musical time
    /// from it. Producer thread only, same exclusivity contract as
    /// [`Self::process_block`].
    ///
    /// Only the tempo-synced delay reads it today, so the other variants fall
    /// through without touching their DSP: this costs a discriminant test per
    /// block, and inside EchoSpace a float compare that exits early unless the
    /// tempo actually moved.
    fn set_transport_tempo(&self, tempo_bpm: f32) {
        // SAFETY: the dedicated producer thread is the sole DSP accessor.
        if let BuiltinDsp::Echospace(dsp) = unsafe { &mut *self.dsp.get() } {
            dsp.set_tempo_bpm(tempo_bpm);
        }
    }

    /// Capture one block into the analyser. Producer thread only. Cheap by
    /// construction — a mono sum into a preallocated ring, no transform.
    fn capture_spectrum(&self, left: &[f32], right: &[f32]) {
        // SAFETY: the dedicated producer thread is the sole accessor.
        unsafe { &mut *self.spectrum.get() }.push_block(left, right);
    }

    /// Run an analysis if one is due, returning the frame to publish. `None`
    /// most blocks: the analyser transforms at its own ~30 Hz rate, not per
    /// block. Producer thread only.
    fn analyze_spectrum(&self) -> Option<[f32; SPECTRUM_BINS]> {
        // SAFETY: the dedicated producer thread is the sole accessor.
        unsafe { &mut *self.spectrum.get() }.analyze().copied()
    }

    /// Latest telemetry frame, or `None` for a built-in that measures nothing.
    /// A core without meters publishes no frame rather than a zeroed one, so
    /// the editor cannot mistake "not measured" for "silence". Producer thread
    /// only (same contract as `process_block`).
    fn meter_frame(&self) -> Option<SpherePluginHost::audio_bridge::BuiltinMeterFrame> {
        // SAFETY: the dedicated producer thread is the sole DSP accessor.
        match unsafe { &*self.dsp.get() } {
            BuiltinDsp::Rodhareist(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    // The multi-FX chain has a compressor stage, but its
                    // reduction is not surfaced separately today.
                    gain_reduction_db: 0.0,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // Single fixed stage, not a user-ordered rack.
                    slot_in_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                    slot_out_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                })
            }
            BuiltinDsp::Fa2a(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // Single fixed stage, not a user-ordered rack.
                    slot_in_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                    slot_out_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                })
            }
            BuiltinDsp::Fa76(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // Single fixed stage, not a user-ordered rack.
                    slot_in_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                    slot_out_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                })
            }
            BuiltinDsp::BurnLimit(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // Single fixed stage, not a user-ordered rack.
                    slot_in_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                    slot_out_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                })
            }
            BuiltinDsp::Clipper67(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // Single fixed stage, not a user-ordered rack.
                    slot_in_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                    slot_out_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                })
            }
            BuiltinDsp::Zcomp(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // Single fixed stage, not a user-ordered rack.
                    slot_in_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                    slot_out_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                })
            }
            BuiltinDsp::MixStation(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // MixStation is the one built-in with a user-ordered rack,
                    // so it publishes a level per position for its row meters.
                    slot_in_peak: f.slot_in_peak,
                    slot_out_peak: f.slot_out_peak,
                })
            }
            BuiltinDsp::Transient(dsp) => {
                let f = dsp.meter_frame();
                Some(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                    in_peak: f.in_peak,
                    in_rms: f.in_rms,
                    out_peak: f.out_peak,
                    out_rms: f.out_rms,
                    // Transient shaping is a gain change in either direction;
                    // the frame carries its magnitude.
                    gain_reduction_db: f.gain_reduction_db,
                    in_clip: f.in_clip,
                    out_clip: f.out_clip,
                    // Single fixed stage, not a user-ordered rack.
                    slot_in_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                    slot_out_peak: [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS],
                })
            }
            BuiltinDsp::Equz8(_)
            | BuiltinDsp::Verbspace(_)
            | BuiltinDsp::Echospace(_)
            | BuiltinDsp::WrapSynth(_) => None,
        }
    }

    /// Reported latency in samples. For Rodhareist: the NAM capture's receptive
    /// field plus the cabinet IR's convolution partition, each counted only
    /// while the stage carrying it is actually in the path. EQ-Z8 is a cascade
    /// of direct-form biquads and adds none; VerbSpace's pre-delay is a musical
    /// parameter, not a processing delay to compensate. Producer thread only.
    fn latency_samples(&self) -> usize {
        // SAFETY: the dedicated producer thread is the sole DSP accessor.
        match unsafe { &*self.dsp.get() } {
            BuiltinDsp::Rodhareist(dsp) => dsp.latency_samples(),
            BuiltinDsp::Equz8(_) => 0,
            BuiltinDsp::Verbspace(dsp) => dsp.latency_samples(),
            BuiltinDsp::Fa2a(dsp) => dsp.latency_samples(),
            BuiltinDsp::Fa76(dsp) => dsp.latency_samples(),
            BuiltinDsp::BurnLimit(dsp) => dsp.latency_samples(),
            BuiltinDsp::Clipper67(dsp) => dsp.latency_samples(),
            BuiltinDsp::Transient(dsp) => dsp.latency_samples(),
            BuiltinDsp::Echospace(dsp) => dsp.latency_samples(),
            BuiltinDsp::WrapSynth(_) => 0,
            BuiltinDsp::Zcomp(dsp) => dsp.latency_samples(),
            BuiltinDsp::MixStation(dsp) => dsp.latency_samples(),
        }
    }

    /// Control-rate parameter apply from the shared param ring. Runs only on
    /// the audio producer thread (same exclusivity contract as
    /// [`Self::process_block`]) and is allocation-free in both arms: a
    /// plain-data `Params` write plus coefficient math. Unknown wire indices
    /// are dropped silently — hot path, no logging.
    fn apply_param(&self, param_id: u32, value: f32) {
        // SAFETY: the dedicated producer thread is the sole DSP accessor.
        match unsafe { &mut *self.dsp.get() } {
            BuiltinDsp::Rodhareist(dsp) => {
                if let Some(id) = rodharerist::ui_param_id(param_id) {
                    let _ = dsp.apply_ui_param(id, value);
                }
            }
            // Already the compact wire form the DSP consumes — no id lookup.
            BuiltinDsp::Equz8(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::Verbspace(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::Echospace(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::Fa2a(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::Fa76(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::BurnLimit(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::Clipper67(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::Transient(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::WrapSynth(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::Zcomp(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
            BuiltinDsp::MixStation(dsp) => {
                let _ = dsp.apply_wire_param(param_id, value);
            }
        }
    }

    /// Deliver one MIDI event to an instrument built-in. The event has already
    /// been routed through this exact instance's bounded shared-memory ring.
    fn apply_midi(&self, event: SharedMidiEvent) {
        let BuiltinDsp::WrapSynth(dsp) = (unsafe { &mut *self.dsp.get() }) else {
            return;
        };
        match event.status & 0xf0 {
            0x90 if event.data2 > 0 => dsp.note_on(event.data1, event.data2),
            0x80 | 0x90 => dsp.note_off(event.data1),
            0xb0 if matches!(event.data1, 120 | 123) => dsp.all_notes_off(),
            _ => {}
        }
    }
}

type SharedBuiltinProcessors = Arc<Mutex<Arc<HashMap<String, Arc<BuiltinHostProcessor>>>>>;

/// Live Audio Unit instances, keyed by insert `plugin_instance_id`. Same
/// copy-on-write shape as the built-in map: the producer clones the outer `Arc`
/// once per wake, and load/unload rebuild through `Arc::make_mut`.
type SharedAuProcessors = Arc<Mutex<Arc<HashMap<String, Arc<AuHostProcessor>>>>>;

/// Resolve an instance id to its Audio Unit, if that is the runtime hosting it.
/// Instance-keyed commands use this to answer for AU before falling through to
/// the VST3 preview engine, which is where they used to go unconditionally.
fn au_instance(
    processors: &SharedAuProcessors,
    plugin_instance_id: &str,
) -> Option<Arc<AuHostProcessor>> {
    processors
        .lock()
        .ok()
        .and_then(|map| map.get(plugin_instance_id).cloned())
}

#[cfg(test)]
mod builtin_processor_tests {
    use super::*;

    #[test]
    fn rodhareist_processes_daw_input_to_stereo_output() {
        let processor = BuiltinHostProcessor::rodhareist(48_000, None);
        let in_l = [0.2f32; 32];
        let in_r = [-0.1f32; 32];
        let mut output = [0.0f32; 64];
        processor.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().any(|sample| sample.abs() > 1.0e-6));
    }

    #[test]
    fn rodhareist_reverb_model_wire_reaches_distinct_algorithms() {
        let mut params = rodharerist::default_params();
        params.stage_order = [None; rodharerist::PATH_SLOTS];
        params.stage_order[0] = Some(rodharerist::StageKind::Reverb);
        params.reverb_mix = 100.0;
        params.reverb_decay_s = 4.0;
        let state = rodharerist::RodhareistState::new(params)
            .to_json()
            .expect("state serializes");
        let model_param =
            rodharerist::ui_param_index("reverb_model").expect("reverb model in wire table");

        let render = |model_index: f32| {
            let processor = BuiltinHostProcessor::rodhareist(48_000, Some(&state));
            processor.apply_param(model_param, model_index);
            let mut rendered = Vec::with_capacity(32_768 * 2);
            for block in 0..128 {
                let mut in_l = [0.0f32; 256];
                let mut in_r = [0.0f32; 256];
                if block == 0 {
                    in_l[0] = 1.0;
                    in_r[0] = 0.25;
                }
                let mut output = [0.0f32; 512];
                processor.process_block(&in_l, &in_r, &mut output, 256);
                rendered.extend_from_slice(&output);
            }
            rendered
        };

        let room = render(1.0);
        let hall = render(2.0);
        let difference = room
            .iter()
            .zip(hall.iter())
            .map(|(room, hall)| {
                let delta = room - hall;
                delta * delta
            })
            .sum::<f32>()
            / room.len() as f32;
        assert!(
            difference.sqrt() > 1.0e-4,
            "reverb_model wire did not switch the hosted DSP algorithm"
        );
    }

    /// Wire-index params reach the DSP: powering the unit off through the
    /// shared table's index mutes the output; out-of-range indices are no-ops.
    #[test]
    fn apply_param_routes_wire_indices_into_the_dsp() {
        let processor = BuiltinHostProcessor::rodhareist(48_000, None);
        let power = rodharerist::ui_param_index("power").expect("power in wire table");
        processor.apply_param(power, 0.0);

        let in_l = [0.3f32; 64];
        let in_r = [0.3f32; 64];
        let mut output = [1.0f32; 128];
        processor.process_block(&in_l, &in_r, &mut output, 64);
        // Power off bypasses the chain: output mirrors the dry input.
        assert!(output.iter().all(|sample| sample.is_finite()));

        // Out-of-range index must be a silent no-op.
        processor.apply_param(u32::MAX, 1.0);
        processor.apply_param(rodharerist::UI_PARAM_IDS.len() as u32, 1.0);
        processor.process_block(&in_l, &in_r, &mut output, 64);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    /// A persisted state blob handed to the constructor must configure the DSP
    /// before any processing: power=off state makes the chain a pure bypass,
    /// observable as the output mirroring the dry input exactly.
    #[test]
    fn constructor_state_blob_configures_the_dsp() {
        let mut params = rodharerist::default_params();
        params.power = false;
        let json = rodharerist::RodhareistState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::rodhareist(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "power-off state must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "power-off state must bypass");
        }

        // A corrupt blob must fall back to defaults, not panic.
        let fallback = BuiltinHostProcessor::rodhareist(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    /// Every stem the catalog advertises as bridge-enabled must actually build
    /// here — the two lists are what route an insert into this process.
    #[test]
    fn every_bridge_enabled_stem_constructs() {
        for stem in SpherePluginHost::builtin::AUDIO_BRIDGE_STEMS {
            assert!(
                BuiltinHostProcessor::new(stem, 48_000, None).is_some(),
                "`{stem}` is bridge-enabled but has no DSP in the host"
            );
        }
        assert!(BuiltinHostProcessor::new("compresser", 48_000, None).is_none());
    }

    #[test]
    fn wrapsynth_consumes_instance_midi_and_renders_audio() {
        let processor = BuiltinHostProcessor::wrapsynth(48_000, None);
        processor.apply_midi(SharedMidiEvent {
            status: 0x90,
            data1: 60,
            data2: 110,
            ..SharedMidiEvent::default()
        });
        let silence = [0.0f32; 256];
        let mut output = [0.0f32; 512];
        processor.process_block(&silence, &silence, &mut output, 256);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().any(|sample| sample.abs() > 1.0e-4));

        processor.apply_midi(SharedMidiEvent {
            status: 0x80,
            data1: 60,
            ..SharedMidiEvent::default()
        });
    }

    /// EQ-Z8 shapes the signal rather than passing it: a band with real gain
    /// must change the output, and every wire index must survive the trip.
    #[test]
    fn equz8_processes_and_takes_wire_params() {
        let processor = BuiltinHostProcessor::equz8(48_000, None);
        let in_l = [0.3f32; 64];
        let in_r = [-0.3f32; 64];
        let mut output = [0.0f32; 128];
        processor.process_block(&in_l, &in_r, &mut output, 64);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().any(|sample| sample.abs() > 1.0e-6));

        // Power off is a pure bypass — the clearest observable param effect.
        let power = equz8::ui_param_index("power").expect("power in wire table");
        processor.apply_param(power, 0.0);
        processor.process_block(&in_l, &in_r, &mut output, 64);
        for i in 0..64 {
            assert_eq!(output[i * 2], in_l[i], "power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "power-off must bypass");
        }

        // Out-of-range indices are silent no-ops, not panics.
        processor.apply_param(u32::MAX, 1.0);
        processor.apply_param(equz8::UI_PARAM_IDS.len() as u32, 1.0);
        processor.process_block(&in_l, &in_r, &mut output, 64);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn equz8_restores_state_and_reports_no_latency_or_meters() {
        let mut params = equz8::default_params();
        params.power = false;
        let json = equz8::ipc::Equz8State::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::equz8(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "restored power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "restored power-off must bypass");
        }

        // A biquad cascade is zero-latency, and an EQ measures nothing — the
        // frame is absent rather than a zeroed one the editor would read as
        // real silence.
        assert_eq!(restored.latency_samples(), 0);
        assert!(restored.meter_frame().is_none());
        assert!(restored.nam_loader.is_none());
        assert!(restored.ir_loader.is_none());

        // A corrupt blob falls back to defaults instead of panicking.
        let fallback = BuiltinHostProcessor::equz8(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    /// A reverb's output outlives its input, so the check that matters here is
    /// that the tail keeps arriving across block boundaries — a per-block
    /// rebuild of the DSP would silence it after the first block.
    #[test]
    fn verbspace_processes_and_sustains_a_tail_across_blocks() {
        let processor = BuiltinHostProcessor::verbspace(48_000, None);
        let mix = verbspace::ui_param_index("mix").expect("mix in wire table");
        processor.apply_param(mix, 100.0);

        let in_l = [0.3f32; 64];
        let in_r = [-0.3f32; 64];
        let mut output = [0.0f32; 128];
        for _ in 0..16 {
            processor.process_block(&in_l, &in_r, &mut output, 64);
        }
        assert!(output.iter().all(|sample| sample.is_finite()));

        let silence = [0.0f32; 64];
        let mut tail = 0.0f32;
        for _ in 0..16 {
            processor.process_block(&silence, &silence, &mut output, 64);
            tail = output.iter().fold(tail, |peak, s| peak.max(s.abs()));
        }
        assert!(tail > 1.0e-4, "no tail after the input stopped: {tail}");

        // Power off is a pure bypass — the clearest observable param effect.
        let power = verbspace::ui_param_index("power").expect("power in wire table");
        processor.apply_param(power, 0.0);
        processor.process_block(&in_l, &in_r, &mut output, 64);
        for i in 0..64 {
            assert_eq!(output[i * 2], in_l[i], "power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "power-off must bypass");
        }

        // Out-of-range indices are silent no-ops, not panics.
        processor.apply_param(u32::MAX, 1.0);
        processor.apply_param(verbspace::UI_PARAM_IDS.len() as u32, 1.0);
        processor.process_block(&in_l, &in_r, &mut output, 64);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn verbspace_restores_state_and_reports_no_latency_or_meters() {
        let mut params = verbspace::default_params();
        params.power = false;
        let json = verbspace::ipc::VerbspaceState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::verbspace(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "restored power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "restored power-off must bypass");
        }

        // The pre-delay is a musical parameter, not lookahead, and the reverb
        // measures nothing — the meter frame is absent rather than zeroed.
        assert_eq!(restored.latency_samples(), 0);
        assert!(restored.meter_frame().is_none());
        assert!(restored.nam_loader.is_none());
        assert!(restored.ir_loader.is_none());

        // A corrupt blob falls back to defaults instead of panicking.
        let fallback = BuiltinHostProcessor::verbspace(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    /// Like the reverb, a delay's output outlives its input: the check that
    /// matters is that repeats keep arriving across block boundaries, which a
    /// per-block DSP rebuild would silence after the first one.
    #[test]
    fn echospace_repeats_survive_block_boundaries() {
        let processor = BuiltinHostProcessor::echospace(48_000, None);
        let mix = echospace::ui_param_index("mix").expect("mix in wire table");
        let time_l = echospace::ui_param_index("timeMsL").expect("timeMsL in wire table");
        let time_r = echospace::ui_param_index("timeMsR").expect("timeMsR in wire table");
        processor.apply_param(mix, 100.0);
        // Longer than one 64-frame block, so the echo can only be heard if the
        // ring survives between calls.
        processor.apply_param(time_l, 40.0);
        processor.apply_param(time_r, 40.0);

        let in_l = [0.3f32; 64];
        let in_r = [-0.3f32; 64];
        let mut output = [0.0f32; 128];
        for _ in 0..16 {
            processor.process_block(&in_l, &in_r, &mut output, 64);
        }
        assert!(output.iter().all(|sample| sample.is_finite()));

        let silence = [0.0f32; 64];
        let mut tail = 0.0f32;
        for _ in 0..16 {
            processor.process_block(&silence, &silence, &mut output, 64);
            tail = output.iter().fold(tail, |peak, s| peak.max(s.abs()));
        }
        assert!(tail > 1.0e-4, "no repeats after the input stopped: {tail}");

        // Power off is a pure bypass — the clearest observable param effect.
        let power = echospace::ui_param_index("power").expect("power in wire table");
        processor.apply_param(power, 0.0);
        processor.process_block(&in_l, &in_r, &mut output, 64);
        for i in 0..64 {
            assert_eq!(output[i * 2], in_l[i], "power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "power-off must bypass");
        }

        // Out-of-range indices are silent no-ops, not panics.
        processor.apply_param(u32::MAX, 1.0);
        processor.apply_param(echospace::UI_PARAM_IDS.len() as u32, 1.0);
        processor.process_block(&in_l, &in_r, &mut output, 64);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn echospace_restores_state_and_reports_no_latency_or_meters() {
        let mut params = echospace::default_params();
        params.power = false;
        let json = echospace::ipc::EchospaceState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::echospace(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "restored power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "restored power-off must bypass");
        }

        // The delay time is a musical parameter, not lookahead, and the delay
        // measures nothing — the meter frame is absent rather than zeroed.
        assert_eq!(restored.latency_samples(), 0);
        assert!(restored.meter_frame().is_none());
        assert!(restored.nam_loader.is_none());
        assert!(restored.ir_loader.is_none());

        // A corrupt blob falls back to defaults instead of panicking.
        let fallback = BuiltinHostProcessor::echospace(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    /// Unlike the other bridged built-ins, FA-2A publishes a meter frame — the
    /// editor's VU needle is driven by it, so the host has to actually carry
    /// the reduction figure rather than a zero.
    #[test]
    fn fa2a_publishes_real_gain_reduction_through_the_host_frame() {
        let processor = BuiltinHostProcessor::fa2a(48_000, None);
        let reduction = fa2a::ui_param_index("peakReduction").expect("in wire table");
        processor.apply_param(reduction, 90.0);

        // A block of silence first: nothing over the threshold, nothing taken
        // off, but a frame is still published.
        let silence = [0.0f32; 64];
        let mut output = [0.0f32; 128];
        processor.process_block(&silence, &silence, &mut output, 64);
        let quiet = processor
            .meter_frame()
            .expect("fa2a always publishes a frame");
        assert!(quiet.gain_reduction_db < 1.0);

        // Then a hot signal for long enough for the envelope to settle.
        let loud = [0.7f32; 64];
        for _ in 0..64 {
            processor.process_block(&loud, &loud, &mut output, 64);
        }
        let frame = processor
            .meter_frame()
            .expect("fa2a always publishes a frame");
        assert!(
            frame.gain_reduction_db > 1.0,
            "no reduction reported for a hot signal: {}",
            frame.gain_reduction_db
        );
        assert!(frame.in_rms > 0.0 && frame.out_rms > 0.0);
        assert_eq!(processor.latency_samples(), 0);
    }

    #[test]
    fn zcomp_publishes_real_gain_reduction_through_the_host_frame() {
        let processor = BuiltinHostProcessor::zcomp(48_000, None);
        let threshold = zcomp::ui_param_index("thresholdDb").expect("in wire table");
        let ratio = zcomp::ui_param_index("ratio").expect("in wire table");
        processor.apply_param(threshold, -30.0);
        processor.apply_param(ratio, 8.0);

        let silence = [0.0f32; 64];
        let mut output = [0.0f32; 128];
        processor.process_block(&silence, &silence, &mut output, 64);
        let quiet = processor
            .meter_frame()
            .expect("zcomp always publishes a frame");
        assert!(quiet.gain_reduction_db < 1.0);

        let loud = [0.75f32; 64];
        for _ in 0..64 {
            processor.process_block(&loud, &loud, &mut output, 64);
        }
        let frame = processor
            .meter_frame()
            .expect("zcomp always publishes a frame");
        assert!(
            frame.gain_reduction_db > 1.0,
            "no reduction reported for a hot signal: {}",
            frame.gain_reduction_db
        );
        assert!(frame.in_rms > 0.0 && frame.out_rms > 0.0);
        assert_eq!(processor.latency_samples(), 0);
    }

    #[test]
    fn zcomp_restores_state_and_takes_wire_params() {
        let mut params = zcomp::default_params();
        params.power = false;
        let json = zcomp::ipc::ZcompState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::zcomp(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i]);
            assert_eq!(output[i * 2 + 1], in_r[i]);
        }

        restored.apply_param(zcomp::ui_param_index("power").expect("power"), 1.0);
        restored.apply_param(zcomp::ui_param_index("model").expect("model"), 1.0);
        restored.apply_param(zcomp::UI_PARAM_IDS.len() as u32, 1.0);
        restored.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));

        let fallback = BuiltinHostProcessor::zcomp(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn mixstation_publishes_its_complete_meter_frame() {
        let processor = BuiltinHostProcessor::mixstation(48_000, None);
        processor.apply_param(
            mixstation::ui_param_index("compEnabled").expect("compressor in wire table"),
            1.0,
        );
        processor.apply_param(
            mixstation::ui_param_index("slot1Module").expect("slot in wire table"),
            3.0,
        );
        processor.apply_param(
            mixstation::ui_param_index("compThresholdDb").expect("threshold in wire table"),
            -30.0,
        );
        processor.apply_param(
            mixstation::ui_param_index("compRatio").expect("ratio in wire table"),
            10.0,
        );

        let loud = [0.75f32; 64];
        let mut output = [0.0f32; 128];
        for _ in 0..64 {
            processor.process_block(&loud, &loud, &mut output, 64);
        }
        let frame = processor
            .meter_frame()
            .expect("mixstation always publishes a frame");
        assert!(frame.in_peak > 0.0);
        assert!(frame.in_rms > 0.0);
        assert!(frame.out_peak > 0.0);
        assert!(frame.out_rms > 0.0);
        assert!(frame.gain_reduction_db > 1.0);
        assert!(!frame.in_clip);
        assert!(!frame.out_clip);
        assert_eq!(processor.latency_samples(), 0);
    }

    #[test]
    fn mixstation_restores_state_and_takes_wire_params() {
        let mut params = mixstation::default_params();
        params.power = false;
        let json = mixstation::ipc::MixStationState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::mixstation(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i]);
            assert_eq!(output[i * 2 + 1], in_r[i]);
        }

        restored.apply_param(
            mixstation::ui_param_index("power").expect("power in wire table"),
            1.0,
        );
        restored.apply_param(
            mixstation::ui_param_index("inputTrimDb").expect("trim in wire table"),
            6.0,
        );
        restored.apply_param(mixstation::UI_PARAM_IDS.len() as u32, 1.0);
        restored.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));

        let fallback = BuiltinHostProcessor::mixstation(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn mixstation_restored_trim_is_active_on_the_first_sample() {
        let mut params = mixstation::default_params();
        params.input_trim_db = 6.0;
        params.filters_enabled = false;
        params.eq_enabled = false;
        params.comp_enabled = false;
        params.sat_enabled = false;
        params.width_enabled = false;
        params.limiter_enabled = false;
        let json = mixstation::ipc::MixStationState::new(params)
            .to_json()
            .expect("state serializes");
        let restored = BuiltinHostProcessor::mixstation(48_000, Some(&json));
        let mut output = [0.0f32; 2];
        restored.process_block(&[0.25], &[0.25], &mut output, 1);
        let expected = 0.25 * builtin_dsp_core::db_to_linear(6.0);
        assert!((output[0] - expected).abs() < 1.0e-6);
        assert!((output[1] - expected).abs() < 1.0e-6);
    }

    #[test]
    fn fa2a_restores_state_and_takes_wire_params() {
        let mut params = fa2a::default_params();
        params.power = false;
        let json = fa2a::ipc::Fa2aState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::fa2a(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "restored power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "restored power-off must bypass");
        }
        // Bypassed still meters, but reports nothing taken off.
        let frame = restored.meter_frame().expect("frame is published");
        assert_eq!(frame.gain_reduction_db, 0.0);

        // Out-of-range indices are silent no-ops, not panics.
        restored.apply_param(u32::MAX, 1.0);
        restored.apply_param(fa2a::UI_PARAM_IDS.len() as u32, 1.0);
        restored.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));

        // A corrupt blob falls back to defaults instead of panicking.
        let fallback = BuiltinHostProcessor::fa2a(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    /// FA-76 publishes a meter frame like FA-2A — the FET editor's blue VU is
    /// driven by real gain reduction from the host.
    #[test]
    fn fa76_publishes_real_gain_reduction_through_the_host_frame() {
        let processor = BuiltinHostProcessor::fa76(48_000, None);
        let input = fa76::ui_param_index("inputDb").expect("in wire table");
        let ratio = fa76::ui_param_index("ratio").expect("in wire table");
        processor.apply_param(input, 30.0);
        processor.apply_param(ratio, fa76::RatioButton::R20.to_wire());

        let silence = [0.0f32; 64];
        let mut output = [0.0f32; 128];
        processor.process_block(&silence, &silence, &mut output, 64);
        let quiet = processor
            .meter_frame()
            .expect("fa76 always publishes a frame");
        assert!(quiet.gain_reduction_db < 1.0);

        let loud = [0.7f32; 64];
        for _ in 0..64 {
            processor.process_block(&loud, &loud, &mut output, 64);
        }
        let frame = processor
            .meter_frame()
            .expect("fa76 always publishes a frame");
        assert!(
            frame.gain_reduction_db > 1.0,
            "no reduction reported for a hot signal: {}",
            frame.gain_reduction_db
        );
        assert!(frame.in_rms > 0.0 && frame.out_rms > 0.0);
        assert_eq!(processor.latency_samples(), 0);
    }

    #[test]
    fn fa76_restores_state_and_takes_wire_params() {
        let mut params = fa76::default_params();
        params.power = false;
        let json = fa76::ipc::Fa76State::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::fa76(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "restored power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "restored power-off must bypass");
        }
        let frame = restored.meter_frame().expect("frame is published");
        assert_eq!(frame.gain_reduction_db, 0.0);

        restored.apply_param(u32::MAX, 1.0);
        restored.apply_param(fa76::UI_PARAM_IDS.len() as u32, 1.0);
        restored.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));

        let fallback = BuiltinHostProcessor::fa76(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn burnlimit_publishes_real_gain_reduction_through_the_host_frame() {
        let processor = BuiltinHostProcessor::burnlimit(48_000, None);
        let gain = burnlimit::ui_param_index("gainDb").expect("in wire table");
        let ceiling = burnlimit::ui_param_index("ceilingDb").expect("in wire table");
        processor.apply_param(gain, 12.0);
        processor.apply_param(ceiling, -1.0);

        let silence = [0.0f32; 64];
        let mut output = [0.0f32; 128];
        processor.process_block(&silence, &silence, &mut output, 64);
        let quiet = processor
            .meter_frame()
            .expect("burnlimit always publishes a frame");
        assert!(quiet.gain_reduction_db < 1.0);

        let loud = [0.8f32; 64];
        for _ in 0..128 {
            processor.process_block(&loud, &loud, &mut output, 64);
        }
        let frame = processor
            .meter_frame()
            .expect("burnlimit always publishes a frame");
        assert!(
            frame.gain_reduction_db > 1.0,
            "no reduction reported for a hot signal: {}",
            frame.gain_reduction_db
        );
        assert!(frame.in_rms > 0.0);
        assert!(processor.latency_samples() > 0);
    }

    #[test]
    fn burnlimit_restores_state_and_takes_wire_params() {
        let mut params = burnlimit::default_params();
        params.power = false;
        let json = burnlimit::ipc::BurnLimitState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::burnlimit(48_000, Some(&json));
        let latency = restored.latency_samples();
        let frames = latency + 32;
        let in_l = vec![0.25f32; frames];
        let in_r = vec![-0.5f32; frames];
        let mut output = vec![0.0f32; frames * 2];
        restored.process_block(&in_l, &in_r, &mut output, frames);
        for i in 0..latency {
            assert_eq!(output[i * 2], 0.0, "bypass must preserve host latency");
            assert_eq!(output[i * 2 + 1], 0.0, "bypass must preserve host latency");
        }
        for i in latency..frames {
            assert_eq!(
                output[i * 2],
                in_l[i - latency],
                "restored power-off must bypass after latency"
            );
            assert_eq!(
                output[i * 2 + 1],
                in_r[i - latency],
                "restored power-off must bypass after latency"
            );
        }
        let frame = restored.meter_frame().expect("frame is published");
        assert_eq!(frame.gain_reduction_db, 0.0);

        restored.apply_param(u32::MAX, 1.0);
        restored.apply_param(burnlimit::UI_PARAM_IDS.len() as u32, 1.0);
        restored.process_block(&in_l, &in_r, &mut output, frames);
        assert!(output.iter().all(|sample| sample.is_finite()));

        let fallback = BuiltinHostProcessor::burnlimit(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, frames);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn clipper67_publishes_real_gain_reduction_through_the_host_frame() {
        let processor = BuiltinHostProcessor::clipper67(48_000, None);
        let threshold = clipper67::ui_param_index("thresholdDb").expect("in wire table");
        let ceiling = clipper67::ui_param_index("ceilingDb").expect("in wire table");
        let mode = clipper67::ui_param_index("mode").expect("in wire table");
        let dc = clipper67::ui_param_index("dcFilter").expect("in wire table");
        processor.apply_param(mode, clipper67::Mode::Limit.to_wire());
        processor.apply_param(dc, 0.0);
        processor.apply_param(threshold, -12.0);
        processor.apply_param(ceiling, -1.0);

        let silence = [0.0f32; 64];
        let mut output = [0.0f32; 128];
        processor.process_block(&silence, &silence, &mut output, 64);
        let quiet = processor
            .meter_frame()
            .expect("clipper67 always publishes a frame");
        assert!(quiet.gain_reduction_db < 1.0);

        // Alternating polarity so the optional DC blocker cannot erase the tone.
        let mut loud = [0.0f32; 64];
        for (i, sample) in loud.iter_mut().enumerate() {
            *sample = if i % 2 == 0 { 0.9 } else { -0.9 };
        }
        for _ in 0..128 {
            processor.process_block(&loud, &loud, &mut output, 64);
        }
        let frame = processor
            .meter_frame()
            .expect("clipper67 always publishes a frame");
        assert!(
            frame.gain_reduction_db > 0.5,
            "no reduction reported for a hot signal: {}",
            frame.gain_reduction_db
        );
        assert!(frame.in_rms > 0.0);
    }

    #[test]
    fn clipper67_restores_state_and_takes_wire_params() {
        let mut params = clipper67::default_params();
        params.power = false;
        let json = clipper67::ipc::Clipper67State::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::clipper67(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "restored power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "restored power-off must bypass");
        }
        let frame = restored.meter_frame().expect("frame is published");
        assert_eq!(frame.gain_reduction_db, 0.0);

        restored.apply_param(u32::MAX, 1.0);
        restored.apply_param(clipper67::UI_PARAM_IDS.len() as u32, 1.0);
        restored.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));

        let fallback = BuiltinHostProcessor::clipper67(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn transient_publishes_real_shaping_through_the_host_frame() {
        let processor = BuiltinHostProcessor::transient(48_000, None);
        let attack = transient::ui_param_index("attack").expect("in wire table");
        let sustain = transient::ui_param_index("sustain").expect("in wire table");
        processor.apply_param(attack, 80.0);
        processor.apply_param(sustain, -40.0);

        let silence = [0.0f32; 64];
        let mut output = [0.0f32; 128];
        processor.process_block(&silence, &silence, &mut output, 64);
        let quiet = processor
            .meter_frame()
            .expect("transient always publishes a frame");
        assert!(quiet.gain_reduction_db < 1.0);

        let mut impulse = [0.0f32; 64];
        impulse[0] = 0.9;
        for _ in 0..32 {
            processor.process_block(&impulse, &impulse, &mut output, 64);
        }
        let frame = processor
            .meter_frame()
            .expect("transient always publishes a frame");
        assert!(
            frame.gain_reduction_db > 0.2,
            "no shaping reported for an impulse: {}",
            frame.gain_reduction_db
        );
        assert!(frame.in_peak > 0.0);
    }

    #[test]
    fn transient_restores_state_and_takes_wire_params() {
        let mut params = transient::default_params();
        params.power = false;
        let json = transient::ipc::TransientState::new(params)
            .to_json()
            .expect("state serializes");

        let restored = BuiltinHostProcessor::transient(48_000, Some(&json));
        let in_l = [0.25f32; 32];
        let in_r = [-0.5f32; 32];
        let mut output = [0.0f32; 64];
        restored.process_block(&in_l, &in_r, &mut output, 32);
        for i in 0..32 {
            assert_eq!(output[i * 2], in_l[i], "restored power-off must bypass");
            assert_eq!(output[i * 2 + 1], in_r[i], "restored power-off must bypass");
        }
        let frame = restored.meter_frame().expect("frame is published");
        assert_eq!(frame.gain_reduction_db, 0.0);

        restored.apply_param(u32::MAX, 1.0);
        restored.apply_param(transient::UI_PARAM_IDS.len() as u32, 1.0);
        restored.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));

        let fallback = BuiltinHostProcessor::transient(48_000, Some("not json"));
        fallback.process_block(&in_l, &in_r, &mut output, 32);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct PendingEditorPrepare {
    prepare_id: u64,
    preferred_width: u32,
    preferred_height: u32,
}

type PendingPrepareRegistry = HashMap<String, PendingEditorPrepare>;

struct DelayedGpuRedraw {
    instance_id: String,
    deadline: Instant,
    second_resize: Option<(u32, u32)>,
}

// Browser/WebView-backed editors can take well over 10s to finish their first
// attach (runtime spin-up + compositor handshake). The whole open is async and
// the app stays fully responsive while we wait, so use a generous bound — long
// enough not to abort a slow-but-healthy editor, still bounded so a genuine
// deadlock fails cleanly instead of hanging forever.
const EDITOR_CREATE_TIMEOUT: Duration = Duration::from_millis(30_000);
const EDITOR_ATTACH_TIMEOUT: Duration = Duration::from_millis(30_000);
const MIN_EDITOR_ATTACH_SIZE: u32 = 16;
const MODULE_LOAD_TIMEOUT: Duration = Duration::from_millis(15_000);
const CREATE_INSTANCE_TIMEOUT: Duration = Duration::from_millis(15_000);
const INITIALIZE_TIMEOUT: Duration = Duration::from_millis(20_000);

#[derive(Debug, Clone)]
struct PendingEditorAttach {
    request_id: u64,
    plugin_instance_id: String,
    parent_hwnd: u64,
    display_title: String,
    started_at: Instant,
    stage: &'static str,
    timeout_logged: bool,
    processor: DirectAudio::vst3_processor::Vst3RuntimeProcessor,
}

struct EditorAttachResult {
    request_id: u64,
    plugin_instance_id: String,
    processor: DirectAudio::vst3_processor::Vst3RuntimeProcessor,
    handle: Option<u64>,
    attach_hwnd: u64,
    owner_hwnd: u64,
    display_title: String,
    preferred_width: u32,
    preferred_height: u32,
    resizable: bool,
    error: Option<String>,
    elapsed: Duration,
}

#[derive(Debug, Clone)]
struct PendingPluginLoad {
    request_id: u64,
    plugin_instance_id: String,
    plugin_path: String,
    class_id: String,
    name: String,
    sample_rate: u32,
    max_block_size: u32,
    started_at: Instant,
    stage: &'static str,
}

struct PluginLoadResult {
    request_id: u64,
    plugin_instance_id: String,
    plugin_path: String,
    class_id: String,
    name: String,
    processor: Option<DirectAudio::vst3_processor::Vst3RuntimeProcessor>,
    error: Option<String>,
    elapsed: Duration,
}

static IDLE_TICK: AtomicU64 = AtomicU64::new(0);
static NEXT_EDITOR_ATTACH_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PLUGIN_LOAD_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn parent_watchdog(parent_pid: u32, shutdown: Arc<AtomicBool>) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if !platform::is_process_alive(parent_pid) {
            eprintln!("[plugin-host] parent process gone; shutting down");
            shutdown.store(true, Ordering::SeqCst);
            break;
        }
    }
}

fn shutdown_host(
    registry: &mut Registry,
    loaded: &mut LoadedRegistry,
    pending: &mut PendingPrepareRegistry,
    preview: &SharedPluginHostPreview,
    reason: &str,
) {
    let editor_count = registry.len();
    eprintln!("[plugin-host] closing editors count={editor_count} reason={reason}");
    {
        let mut engine = preview.lock();
        for instance_id in registry.drain().map(|(id, _)| id) {
            engine.embed_detach_for_instance(&instance_id);
            engine.unload_instance(&instance_id);
        }
    }
    pending.clear();
    let plugin_count = loaded.len();
    eprintln!("[plugin-host] unloading plugins count={plugin_count}");
    loaded.clear();
    eprintln!("[plugin-host] process exit");
}

/// Stage 3 (host producer side): if the engine has requested a new block via
/// `request_seq`, drain the shared MIDI ring into the loaded VSTi, render one
/// block, write it to `audio_out`, publish the output meters, and acknowledge
/// with `done_seq`. Wait-free handshake — the host never blocks the engine; the
/// engine reads whatever the host last produced (one-block latency).
///
/// Runs on the dedicated audio producer thread (`run_audio_producer`), woken
/// by the engine's kick event per requested block. Removing the
/// `render_single_voice` Vec allocs is a remaining refinement.
/// Which runtime owns the instance whose block is being produced.
///
/// Resolved once per block at the producer's call site so the branches below
/// never re-derive the format. Adding a variant here deliberately breaks every
/// `match` in `service_audio_bridge`, which is the list of questions a new
/// runtime has to answer: MIDI, parameters, channel count, process, readiness,
/// and latency.
#[derive(Copy, Clone)]
enum BlockRuntime<'a> {
    /// Hosted by `BridgeAudioShared`, looked up by instance id.
    Vst3,
    /// A Futureboard built-in DSP owned by this process.
    Builtin(&'a BuiltinHostProcessor),
    /// A macOS Audio Unit owned by this process.
    Au(&'a AuHostProcessor),
}

fn service_audio_bridge(
    region: &SharedAudioRegion,
    dsp: &BridgeAudioShared,
    runtime: BlockRuntime<'_>,
    plugin_instance_id: &str,
) {
    let bridge = region.bridge();
    let req = bridge.request_seq.load(Ordering::Acquire);
    let done = bridge.done_seq.load(Ordering::Relaxed);
    if req == done {
        return; // no new block requested
    }
    let process_started = Instant::now();
    // No engine mutex on the block path: the voice list is an Arc snapshot the
    // engine republishes on load/unload only, so the IPC thread can hold the
    // engine lock across editor attach / plugin load for seconds without
    // starving block production (the old `lock_misses` dropouts).
    let frames = (bridge.block_frames.load(Ordering::Relaxed) as usize).min(MAX_BLOCK_FRAMES);
    // Drain engine-pushed MIDI. Each region's ring belongs to exactly one
    // insert instance, so events are routed to that voice only — never
    // broadcast to every loaded plugin.
    let mut midi_count = 0u32;
    while let Some(ev) = bridge.midi.try_pop() {
        match runtime {
            BlockRuntime::Builtin(builtin) => builtin.apply_midi(ev),
            BlockRuntime::Vst3 => dsp.apply_shared_midi(plugin_instance_id, ev),
            // Audio Units schedule events inside the slice, so the ring's
            // `sample_offset` survives the hop (the built-in path drops it).
            BlockRuntime::Au(au) => au.apply_midi(ev),
        }
        midi_count += 1;
    }
    // stderr is a pipe to the parent process — writing it from the producer
    // thread per block can stall block production if the parent is slow to
    // drain it, so consume traces only exist under the debug flag.
    if midi_count > 0 && debug_enabled() {
        eprintln!(
            "[plugin-host-midi-consume] seq={req} instance={plugin_instance_id} events={midi_count}"
        );
    }
    // Drain engine-pushed parameter automation into this voice (queued for the
    // process() below). Per-instance ring, like MIDI — never broadcast.
    let mut param_count = 0u32;
    while let Some(ev) = bridge.params.try_pop() {
        match runtime {
            // Built-in DSP: apply at block boundary (control-rate; the wire
            // index resolves to an `apply_ui_param` id). `sample_offset` is
            // intentionally ignored.
            BlockRuntime::Builtin(b) => b.apply_param(ev.param_id, ev.value),
            BlockRuntime::Vst3 => dsp.apply_shared_param(plugin_instance_id, ev.param_id, ev.value),
            // Normalized 0..1 like VST3; the AU layer denormalizes through the
            // parameter's own plain min/max.
            BlockRuntime::Au(au) => au.apply_param(ev.param_id, ev.value),
        }
        param_count += 1;
    }
    if param_count > 0 && debug_enabled() {
        eprintln!(
            "[plugin-host-param-consume] seq={req} instance={plugin_instance_id} params={param_count}"
        );
    }
    let mut in_l = [0.0f32; MAX_BLOCK_FRAMES];
    let mut in_r = [0.0f32; MAX_BLOCK_FRAMES];
    // SAFETY: the engine owns `audio_in` until it bumps `request_seq`.
    unsafe {
        bridge
            .audio_in
            .read_deinterleaved(&mut in_l[..frames], &mut in_r[..frames], frames);
    }
    // Analyser capture, before the DSP touches anything: the editor's spectrum
    // overlay shows the signal arriving at the insert. The transport tempo the
    // engine published for this block goes in at the same point, so a built-in
    // that locks to the grid uses the real project tempo rather than a stub.
    if let BlockRuntime::Builtin(b) = runtime {
        b.set_transport_tempo(bridge.load_transport().tempo_bpm as f32);
        b.capture_spectrum(&in_l[..frames], &in_r[..frames]);
    }
    let output_channels = match runtime {
        BlockRuntime::Builtin(_) => 2,
        BlockRuntime::Vst3 => dsp
            .main_audio_output_channel_count_for_instance(plugin_instance_id)
            .unwrap_or_else(|| bridge.plugin_output_channels())
            .max(1)
            .min(MAX_CHANNELS as u32) as usize,
        // Fixed by the stream format negotiated at open, so this needs no
        // instance lookup.
        BlockRuntime::Au(au) => au.output_channels().max(1).min(MAX_CHANNELS as u32) as usize,
    };
    bridge.set_plugin_output_channels(output_channels as u32);
    let mut interleaved = [0.0f32; AUDIO_BUF_LEN];
    let len = (frames * output_channels).min(AUDIO_BUF_LEN);
    let produced_channels = match runtime {
        BlockRuntime::Builtin(builtin) => {
            builtin.process_block(
                &in_l[..frames],
                &in_r[..frames],
                &mut interleaved[..len],
                frames,
            );
            2
        }
        BlockRuntime::Vst3 => {
            // Real transport ProcessContext published by the engine for this block.
            let bt = bridge.load_transport();
            let transport = DirectAudio::RuntimeTransportContext {
                tempo_bpm: bt.tempo_bpm,
                time_sig_num: bt.time_sig_num,
                time_sig_den: bt.time_sig_den,
                project_time_samples: bt.project_time_samples,
                ppq_position: bt.ppq_position,
                bar_position_ppq: bt.bar_position_ppq,
                playing: bt.playing,
                recording: bt.recording,
            };
            let produced = dsp.render_single_voice_interleaved(
                plugin_instance_id,
                frames,
                &in_l[..frames],
                &in_r[..frames],
                &mut interleaved[..len],
                output_channels,
                transport,
            );
            if produced > 0 {
                produced.min(output_channels)
            } else {
                // Missing instances and VST3 process failures are not valid wet
                // blocks. Preserve the track's dry signal instead of publishing
                // a fresh zero buffer that the engine would treat as success.
                let channels = output_channels.min(2);
                for frame in 0..frames {
                    let base = frame * output_channels;
                    interleaved[base] = in_l[frame];
                    if channels > 1 {
                        interleaved[base + 1] = in_r[frame];
                    }
                }
                channels
            }
        }
        BlockRuntime::Au(au) => {
            let bt = bridge.load_transport();
            let transport = AuTransport {
                tempo_bpm: bt.tempo_bpm,
                ppq_position: bt.ppq_position,
                bar_position_ppq: bt.bar_position_ppq,
                project_time_samples: bt.project_time_samples,
                time_sig_num: bt.time_sig_num,
                time_sig_den: bt.time_sig_den,
                playing: i32::from(bt.playing),
                recording: i32::from(bt.recording),
            };
            let produced = au.render(
                &in_l[..frames],
                &in_r[..frames],
                &mut interleaved[..len],
                frames,
                output_channels,
                transport,
            ) as usize;
            if produced > 0 {
                produced.min(output_channels)
            } else {
                // A failed render must not mute the track. Pass the dry signal
                // through instead, the same choice the engine makes when the
                // bridge misses a deadline.
                let channels = output_channels.min(2);
                for frame in 0..frames {
                    let base = frame * output_channels;
                    interleaved[base] = in_l[frame];
                    if channels > 1 {
                        interleaved[base + 1] = in_r[frame];
                    }
                }
                channels
            }
        }
    };
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    for i in 0..frames {
        let base = i * output_channels;
        let l = interleaved[base];
        let r = if produced_channels > 1 {
            interleaved[base + 1]
        } else {
            l
        };
        peak_l = peak_l.max(l.abs());
        peak_r = peak_r.max(r.abs());
    }
    let dsp_ready = match runtime {
        // Both are live the moment their instance is published to the producer.
        BlockRuntime::Builtin(_) | BlockRuntime::Au(_) => true,
        BlockRuntime::Vst3 => {
            dsp.dsp_ready() && (dsp.has_loaded_instances() || dsp.continuous_mode())
        }
    };
    // SAFETY: the host owns `audio_out` for this block — the engine waits on
    // `done_seq` (published below) before reading it.
    unsafe {
        bridge.audio_out.write_interleaved(&interleaved[..len]);
    }
    bridge.store_meters(peak_l, peak_r);
    // Built-in DSP telemetry: publish the DSP's own post-trim meter frame so
    // the editor's meters show what the chain actually saw (producer thread —
    // the sole legal DSP accessor).
    //
    // Analyser frames follow at the analyser's own rate — `analyze_spectrum`
    // returns `None` on the blocks in between, so that publishes ~30 times a
    // second rather than once per block.
    if let BlockRuntime::Builtin(b) = runtime {
        if let Some(frame) = b.meter_frame() {
            bridge.store_builtin_meters(&frame);
        }
        if let Some(bins) = b.analyze_spectrum() {
            bridge.store_spectrum(&bins);
        }
    }
    bridge.set_dsp_output_ready(dsp_ready);
    // Publish the plugin's reported latency so the engine can surface it (and,
    // later, compensate it). Refreshed periodically — latency rarely changes,
    // so this avoids an FFI getter + voice lookup on every block.
    static LATENCY_REPORT_BLOCKS: AtomicU64 = AtomicU64::new(0);
    if LATENCY_REPORT_BLOCKS
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(64)
    {
        let latency = match runtime {
            // A loaded NAM capture's receptive field is the built-in chain's
            // only latency contributor today.
            BlockRuntime::Builtin(b) => Some(b.latency_samples().min(i32::MAX as usize) as i32),
            BlockRuntime::Vst3 => dsp.voice_latency_samples(plugin_instance_id),
            // `kAudioUnitProperty_Latency`, read on the same 64-block cadence.
            BlockRuntime::Au(au) => Some(au.latency_samples().min(i32::MAX as u32) as i32),
        };
        if let Some(latency) = latency {
            bridge
                .latency_samples
                .store(latency as u32, Ordering::Relaxed);
        }
    }
    // Throttled and debug-gated: at most one audible-output trace per ~256
    // blocks, and only when audio-out debugging is on — the producer thread
    // must not write the parent's stderr pipe in normal operation.
    static VST3_PROCESS_LOG_BLOCKS: AtomicU64 = AtomicU64::new(0);
    if debug_audio_out_enabled()
        && (peak_l > 0.0001 || peak_r > 0.0001)
        && VST3_PROCESS_LOG_BLOCKS
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(256)
    {
        eprintln!(
            "[vst3-process] instance={plugin_instance_id} frames={frames} channels={produced_channels} midi_events={midi_count} output_peak_l={peak_l:.6} output_peak_r={peak_r:.6}",
        );
    }
    let process_micros = process_started.elapsed().as_micros().min(u32::MAX as u128) as u32;
    bridge.record_process_timing(frames, process_micros);
    bridge.done_seq.store(req, Ordering::Release);
}

/// All mapped shared-audio regions, keyed by insert `plugin_instance_id`.
/// All mapped shared-audio regions, keyed by insert `plugin_instance_id`.
///
/// Inner `Arc<HashMap>` (copy-on-write): the producer thread clones the outer
/// `Arc` once per wake (a refcount bump) instead of deep-cloning the whole map
/// (with its `String` keys) every block. The map only changes on
/// attach/unload, which rebuild it via `Arc::make_mut`.
type SharedAudioRegions = Arc<Mutex<Arc<HashMap<String, Arc<SharedAudioRegion>>>>>;

/// Raise this thread to pro-audio scheduling before it produces blocks.
///
/// Two fixes for the "plugin missed deadline; bypassing to dry" dropouts:
/// `timeBeginPeriod(1)` raises the **process** timer resolution so the
/// timeout fallback in the kick-event wait ticks at ~1 ms instead of the
/// 15.6 ms default a background process can be left with (per-process since
/// Win10 2004), and MMCSS "Pro Audio" puts the producer in the same scheduling
/// class as the engine's WASAPI callback thread so it is not preempted by
/// ordinary UI work while rendering a block. Both follow the proven pattern in
/// `SphereDirectAudioEngine::backend::wasapi_exclusive`.
#[cfg(windows)]
fn boost_audio_producer_thread() {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(u_period: u32) -> u32;
    }
    #[link(name = "avrt")]
    extern "system" {
        fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
    }
    unsafe {
        let timer_res = timeBeginPeriod(1); // 0 == TIMERR_NOERROR
        let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut task_index = 0u32;
        let mmcss = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index);
        eprintln!(
            "[plugin-host-audio] producer boost mmcss_pro_audio={} timer_period_1ms={}",
            mmcss != 0,
            timer_res == 0
        );
    }
}

/// Promote the producer itself before entering the hot loop. This deliberately
/// runs on the producer thread (once, before any block is serviced), so the
/// policy applies to the thread that actually calls VST3 `process()`.
#[cfg(target_os = "linux")]
fn boost_audio_producer_thread() {
    const FIFO_PRIORITIES: [libc::c_int; 3] = [70, 20, 5];
    const FALLBACK_NICE: libc::c_int = -10;

    for priority in FIFO_PRIORITIES {
        // SAFETY: every field in `sched_param` is initialized before the call,
        // and pid 0 means the calling thread on Linux.
        let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
        param.sched_priority = priority;
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } == 0 {
            eprintln!("[plugin-host-audio] producer scheduling=SCHED_FIFO priority={priority}");
            return;
        }
    }

    // Permission-less fallback. Linux applies PRIO_PROCESS to the individual
    // thread when passed its tid; pid 0 means this producer thread.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, FALLBACK_NICE) } == 0 {
        eprintln!(
            "[plugin-host-audio] realtime permission denied; producer scheduling=SCHED_OTHER nice={FALLBACK_NICE}"
        );
    } else {
        eprintln!(
            "[plugin-host-audio] realtime permission denied and nice fallback unavailable: {}",
            io::Error::last_os_error()
        );
    }
}

/// macOS has no Linux-style per-user RT priority budget. Put the producer in
/// the user-interactive QoS class, then request a bounded Mach time-constraint
/// policy. The QoS promotion remains active when the stricter policy is denied.
#[cfg(target_os = "macos")]
fn boost_audio_producer_thread() {
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    #[repr(C)]
    struct ThreadTimeConstraintPolicy {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: i32,
    }
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        fn mach_thread_self() -> u32;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
        fn thread_policy_set(thread: u32, flavor: i32, policy_info: *const i32, count: u32) -> i32;
    }

    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    const THREAD_TIME_CONSTRAINT_POLICY: i32 = 2;

    // SAFETY: these functions affect only the calling thread and all pointers
    // below refer to initialized, correctly sized C-layout values.
    unsafe {
        let qos_ok = pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) == 0;
        let mut timebase = MachTimebaseInfo { numer: 0, denom: 0 };
        let timebase_ok =
            mach_timebase_info(&mut timebase) == 0 && timebase.numer != 0 && timebase.denom != 0;
        let to_ticks = |nanos: u64| -> u32 {
            if timebase_ok {
                nanos
                    .saturating_mul(timebase.denom as u64)
                    .checked_div(timebase.numer as u64)
                    .unwrap_or(0)
                    .min(u32::MAX as u64) as u32
            } else {
                0
            }
        };
        let policy = ThreadTimeConstraintPolicy {
            period: 0,
            computation: to_ticks(2_000_000),
            constraint: to_ticks(10_000_000),
            preemptible: 1,
        };
        let rt_ok = timebase_ok
            && thread_policy_set(
                mach_thread_self(),
                THREAD_TIME_CONSTRAINT_POLICY,
                (&policy as *const ThreadTimeConstraintPolicy).cast(),
                (std::mem::size_of::<ThreadTimeConstraintPolicy>() / std::mem::size_of::<i32>())
                    as u32,
            ) == 0;
        eprintln!(
            "[plugin-host-audio] producer qos_user_interactive={qos_ok} mach_time_constraint={rt_ok} fallback={}",
            if rt_ok { "none" } else if qos_ok { "qos" } else { "default" }
        );
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn boost_audio_producer_thread() {}

#[derive(Default)]
struct ProducerDiagnostics {
    blocks: u64,
    deadline_misses: u64,
    max_process_micros: u32,
}

impl ProducerDiagnostics {
    fn record(&mut self, bridge: &SpherePluginHost::audio_bridge::SharedAudioBridge) {
        let process_micros = bridge.last_process_micros.load(Ordering::Relaxed);
        let frames = bridge.block_frames.load(Ordering::Relaxed) as u64;
        let sample_rate = bridge.sample_rate.load(Ordering::Relaxed).max(1) as u64;
        let deadline_micros = frames
            .saturating_mul(1_000_000)
            .saturating_add(sample_rate - 1)
            / sample_rate;
        self.blocks = self.blocks.saturating_add(1);
        self.max_process_micros = self.max_process_micros.max(process_micros);
        if deadline_micros > 0 && process_micros as u64 >= deadline_micros {
            self.deadline_misses = self.deadline_misses.saturating_add(1);
        }
    }

    fn report(&self) {
        eprintln!(
            "[plugin-host-audio] producer diagnostics os={} blocks={} deadline_misses={} max_process_us={}",
            std::env::consts::OS,
            self.blocks,
            self.deadline_misses,
            self.max_process_micros
        );
    }
}

/// Dedicated host audio producer thread. VST3 `process()` runs only here (the
/// editor stays on the STA main thread); the per-voice MIDI mutex inside
/// `BridgeAudioShared` serializes it against IPC MIDI so there is never a
/// concurrent `process()` — without coupling block production to the engine
/// mutex held across plugin load / editor attach.
///
/// Cadence: event-driven. The engine signals `kick` after every `request_seq`
/// bump (see `SharedRegionSink::request_block`), so the producer wakes within
/// scheduler latency of each block request instead of polling on a Windows
/// timer tick it cannot trust. The 1 ms wait timeout is a safety net only —
/// it keeps shutdown responsive and sweeps any request whose signal raced the
/// wait. Without a kick event (no `--parent-pid`, or creation failed) it falls
/// back to the legacy 250 µs sleep poll.
/// Stack for the audio producer thread. The block path's own fixed-size
/// buffers are 272 KiB before any plugin DSP is inlined on top, and the
/// platform default (2 MiB) left no usable margin — measured, the thread wanted
/// between 2.0 and 2.5 MiB. 8 MiB costs nothing but address space (stack pages
/// are committed on use) and takes the whole class of failure off the table.
const AUDIO_PRODUCER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn run_audio_producer(
    regions: SharedAudioRegions,
    builtins: SharedBuiltinProcessors,
    audio_units: SharedAuProcessors,
    dsp: Arc<BridgeAudioShared>,
    shutdown: Arc<AtomicBool>,
    kick: Option<BridgeKickEvent>,
) {
    boost_audio_producer_thread();
    let mut diagnostics = ProducerDiagnostics::default();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let snapshot = regions
            .lock()
            .map(|map| Arc::clone(&map))
            .unwrap_or_default();
        if snapshot.is_empty() {
            // With no mapped plug-in audio regions there is no work to poll.
            // Wait longer and avoid locking/cloning the runtime registries at
            // audio cadence. The first engine request signals `kick`; the
            // fallback path wakes within 10 ms when no event is available.
            match &kick {
                Some(kick) => {
                    let _ = kick.wait(20);
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
            continue;
        }
        let builtin_snapshot = builtins
            .lock()
            .map(|map| Arc::clone(&map))
            .unwrap_or_default();
        let au_snapshot = audio_units
            .lock()
            .map(|map| Arc::clone(&map))
            .unwrap_or_default();
        for (instance_id, region) in snapshot.iter() {
            let before = region.bridge().done_seq.load(Ordering::Relaxed);
            // One instance id belongs to exactly one runtime: the load command
            // decided which map it went into.
            let runtime = match (
                builtin_snapshot.get(instance_id),
                au_snapshot.get(instance_id),
            ) {
                (Some(builtin), _) => BlockRuntime::Builtin(builtin.as_ref()),
                (None, Some(au)) => BlockRuntime::Au(au.as_ref()),
                (None, None) => BlockRuntime::Vst3,
            };
            service_audio_bridge(region.as_ref(), &dsp, runtime, instance_id);
            if region.bridge().done_seq.load(Ordering::Relaxed) != before {
                diagnostics.record(region.bridge());
            }
        }
        // Acknowledge the latest voice-snapshot publish now that any snapshot
        // borrowed for this block has been dropped (lets unload hand the final
        // processor release back to the IPC thread).
        dsp.mark_snapshot_observed();
        match &kick {
            // Wakes immediately on the engine's request signal. A request
            // bumped after the region scan above leaves the auto-reset event
            // signaled, so this wait returns without blocking — no lost
            // wakeups.
            Some(kick) => {
                let _ = kick.wait(1);
            }
            None => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    diagnostics.report();
}

fn run_ipc_loop(mut out: io::Stdout, shutdown: Arc<AtomicBool>) {
    // Commands are read on a dedicated thread so the STA/message-pump thread
    // never blocks on stdin (spec Part 9). Each received command kicks the UI
    // thread out of its message wait so IPC latency stays low even while the
    // loop idles in MsgWaitForMultipleObjectsEx.
    let ui_thread_id = platform::current_thread_id();
    let (tx, rx) = crossbeam_channel::unbounded::<HostCommand>();
    std::thread::Builder::new()
        .name("plugin-host-stdin".into())
        .spawn(move || {
            let mut reader = BufReader::new(io::stdin());
            loop {
                match ipc::read_frame::<HostCommand, _>(&mut reader) {
                    Ok(Some(cmd)) => {
                        if tx.send(cmd).is_err() {
                            break;
                        }
                        platform::wake_ui_thread(ui_thread_id);
                    }
                    Ok(None) => {
                        eprintln!("[plugin-host] stdin eof; parent likely exited; shutting down");
                        break;
                    }
                    Err(_) => break,
                }
            }
            platform::wake_ui_thread(ui_thread_id);
        })
        .expect("spawn plugin-host stdin reader");

    let mut registry = Registry::new();
    let mut loaded = LoadedRegistry::new();
    let (load_result_tx, load_result_rx) = crossbeam_channel::unbounded::<PluginLoadResult>();
    let mut pending_plugin_loads: HashMap<String, PendingPluginLoad> = HashMap::new();
    let mut pending_prepare = PendingPrepareRegistry::new();
    let mut delayed_redraws: Vec<DelayedGpuRedraw> = Vec::new();
    let (attach_result_tx, attach_result_rx) = crossbeam_channel::unbounded::<EditorAttachResult>();
    let mut pending_editor_attaches: HashMap<String, PendingEditorAttach> = HashMap::new();
    // Latest requested editor size per instance (coalesced from ResizeEditor
    // commands), applied below with a bounded preview try-lock.
    let mut pending_resizes: HashMap<String, (u32, u32, u32)> = HashMap::new();
    let preview: SharedPluginHostPreview = PluginHostPreviewEngine::shared(48_000, 256);
    let mut preview_output_started = false;
    log_host_audio_mode();
    // Stage 2/3: the mapped shared-memory audio bridge (engine-created), shared
    // with the dedicated audio producer thread. The `Arc` keeps the view mapped
    // for the host's lifetime.
    let region_slots: SharedAudioRegions = Arc::new(Mutex::new(Arc::new(HashMap::new())));
    let builtin_processors: SharedBuiltinProcessors =
        Arc::new(Mutex::new(Arc::new(HashMap::new())));
    let au_processors: SharedAuProcessors = Arc::new(Mutex::new(Arc::new(HashMap::new())));
    {
        let slots = region_slots.clone();
        let builtins = builtin_processors.clone();
        let audio_units = au_processors.clone();
        let dsp = preview.lock().bridge_shared();
        let shutdown = shutdown.clone();
        // Producer wake event, shared with the engine-side sink. Keyed by the
        // engine/host pid pair — both sides derive the same name without any
        // protocol change (the engine knows our pid from spawn, we know its
        // pid from `--parent-pid`).
        let kick = parse_parent_pid().and_then(|parent_pid| {
            let name = bridge_kick_event_name(parent_pid, std::process::id());
            match BridgeKickEvent::create_named(&name) {
                Ok(event) => {
                    eprintln!("[plugin-host-audio] kick event ready name={name}");
                    Some(event)
                }
                Err(error) => {
                    eprintln!(
                        "[plugin-host-audio] kick event create failed name={name} error={error}; falling back to 250us poll"
                    );
                    None
                }
            }
        });
        std::thread::Builder::new()
            .name("plugin-host-audio".into())
            // `service_audio_bridge` alone puts 272 KiB of fixed-size block
            // buffers on this thread's stack (`in_l`/`in_r` at MAX_BLOCK_FRAMES
            // plus `interleaved` at MAX_BLOCK_FRAMES * MAX_CHANNELS), and the
            // rest of the inlined block path sat just under the 2 MiB default
            // — close enough that adding DSP to the path overflowed it on
            // thread entry, aborting the whole host process before any plugin
            // was even loaded. Ask for a stack that is not a coincidence.
            .stack_size(AUDIO_PRODUCER_STACK_BYTES)
            .spawn(move || run_audio_producer(slots, builtins, audio_units, dsp, shutdown, kick))
            .expect("spawn plugin-host audio producer");
    }

    eprintln!(
        "[PluginUIThread] loop started thread_id={}",
        platform::current_thread_id()
    );
    if platform::editor_safe_mode() {
        eprintln!(
            "[PluginEditorSafe] FUTUREBOARD_PLUGIN_EDITOR_SAFE=1 — window-tree polling, \
             per-message verbose logs, attach-time re-entrant pumping, and focus hacks disabled"
        );
    }
    // Pump-gap watchdog: if message dispatch stalls >50ms while an editor is
    // open, the plugin UI freezes (cross-process parenting attaches input
    // queues, so a wedged host thread blocks clicks on plugin dialogs too).
    let mut last_pump_done = Instant::now();
    let mut window_tree: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    // Spin watchdog state: consecutive wakes that claimed input but dispatched
    // nothing (the signature of a 100% CPU pump spin).
    let mut spin_iterations: u32 = 0;
    let mut spin_window_start = Instant::now();
    let mut last_wait_mode: &'static str = "";

    loop {
        if shutdown.load(Ordering::SeqCst) {
            eprintln!("[PluginUIThread] loop exited reason=parent_watchdog");
            shutdown_host(
                &mut registry,
                &mut loaded,
                &mut pending_prepare,
                &preview,
                "parent_watchdog",
            );
            return;
        }
        let mut slowest_section: &'static str = "none";
        let mut slowest_section_ms: u128 = 0;
        macro_rules! timed_section {
            ($name:expr, $body:expr) => {{
                let started = Instant::now();
                let result = $body;
                let elapsed = started.elapsed().as_millis();
                if elapsed > slowest_section_ms {
                    slowest_section_ms = elapsed;
                    slowest_section = $name;
                }
                result
            }};
        }

        // 1. Drain and dispatch every queued command. Each dispatch is timed:
        //    long handlers (plugin load, editor attach) block this pump and —
        //    because cross-process parenting attaches input queues — block
        //    clicks on plugin windows too. The watchdog below names them.
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if matches!(cmd, HostCommand::Shutdown) {
                        hlog!("[PluginHostEditor] shutdown requested");
                        eprintln!("[PluginUIThread] loop exited reason=ipc_shutdown");
                        shutdown_host(
                            &mut registry,
                            &mut loaded,
                            &mut pending_prepare,
                            &preview,
                            "ipc_shutdown",
                        );
                        return;
                    }
                    timed_section!("ipc_dispatch", {
                        dispatch(
                            cmd,
                            &mut registry,
                            &mut loaded,
                            &mut pending_plugin_loads,
                            &load_result_tx,
                            &mut pending_prepare,
                            &mut delayed_redraws,
                            &mut pending_resizes,
                            &mut pending_editor_attaches,
                            &attach_result_tx,
                            &preview,
                            &mut preview_output_started,
                            &region_slots,
                            &builtin_processors,
                            &au_processors,
                            &mut out,
                        )
                    });
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    eprintln!("[plugin-host] stdin eof; parent likely exited; shutting down");
                    eprintln!("[PluginUIThread] loop exited reason=stdin_disconnect");
                    shutdown_host(
                        &mut registry,
                        &mut loaded,
                        &mut pending_prepare,
                        &preview,
                        "stdin_disconnect",
                    );
                    return;
                }
            }
        }

        timed_section!("plugin_load_results", {
            drain_plugin_load_results(
                &mut loaded,
                &mut pending_plugin_loads,
                &load_result_rx,
                &preview,
                &mut out,
            );
            expire_plugin_load_requests(&mut pending_plugin_loads, &load_result_rx, &mut out);
        });

        timed_section!("editor_attach_results", {
            drain_editor_attach_results(
                &mut registry,
                &mut delayed_redraws,
                &mut pending_editor_attaches,
                &attach_result_rx,
                &preview,
                &mut out,
            );
            expire_editor_attach_requests(
                &mut pending_editor_attaches,
                &attach_result_rx,
                &preview,
                &mut out,
            );
        });

        // 1b. Apply coalesced editor resizes (latest size per instance). The
        //     processor clone is fetched with a bounded try-lock so a busy DSP
        //     block can never stall the pump during an interactive resize;
        //     entries that miss the lock are retried next tick (≤8ms away).
        timed_section!("editor_resize", {
            if !pending_resizes.is_empty() {
                // Clone processor handles under a short bounded lock, then
                // apply the (possibly slow) onSize work with the lock RELEASED
                // so the audio producer never waits on editor UI work.
                type ResizeJob = (
                    String,
                    u32,
                    u32,
                    u32,
                    DirectAudio::vst3_processor::Vst3RuntimeProcessor,
                );
                let jobs: Option<(Vec<ResizeJob>, Vec<String>)> = preview
                    .try_lock_for(Duration::from_millis(2))
                    .map(|engine| {
                        let mut jobs = Vec::new();
                        let mut gone = Vec::new();
                        for (instance_id, (width, height, dpi)) in pending_resizes.iter() {
                            match engine.clone_processor_for(instance_id) {
                                Some(processor) => jobs.push((
                                    instance_id.clone(),
                                    *width,
                                    *height,
                                    *dpi,
                                    processor,
                                )),
                                None => gone.push(instance_id.clone()),
                            }
                        }
                        (jobs, gone)
                    });
                if let Some((jobs, gone)) = jobs {
                    for instance_id in gone {
                        pending_resizes.remove(&instance_id); // unloaded — drop request
                    }
                    for (instance_id, width, height, dpi, processor) in jobs {
                        eprintln!(
                            "[plugin-bridge] ResizeEditor instance={instance_id} \
                             width={width} height={height} dpi={dpi}"
                        );
                        processor.view_set_size(width as i32, height as i32);
                        pending_resizes.remove(&instance_id);
                    }
                }
            }
        });

        // 2. Keep attached editors painting / geometry in sync, and pump our own
        //    message queue so the foreign-parented IPlugView gets messages.
        //    The preview-engine mutex is shared with the DSP producer thread —
        //    this UI thread must NEVER block on it inside the pump path: use
        //    short bounded try-locks and skip the tick when the lock is busy.
        let mut chrome_actions: Vec<(String, i32, i32, String)> = Vec::new();
        let user_closed_editors: Vec<String> = timed_section!("editor_refresh", {
            let mut user_closed: Vec<String> = Vec::new();
            let refresh_targets: Option<
                Vec<(String, DirectAudio::vst3_processor::Vst3RuntimeProcessor)>,
            > = preview
                .try_lock_for(Duration::from_millis(2))
                .map(|engine| {
                    registry
                        .keys()
                        .filter_map(|id| {
                            engine
                                .clone_processor_for(id)
                                .map(|processor| (id.clone(), processor))
                        })
                        .collect()
                });
            if let Some(refresh_targets) = refresh_targets {
                for (instance_id, processor) in refresh_targets {
                    // Host-owned (detached) window: the user can close it via its
                    // own titlebar. Detect that here and report EditorClosed so
                    // the main app drops the session and Open works again. The
                    // audio instance stays alive (we only detach the editor).
                    if processor.embed_take_user_close() {
                        user_closed.push(instance_id.clone());
                        continue;
                    }
                    // Chrome presses queued by the AppKit strip since the last
                    // tick. Collected here rather than reported from the strip
                    // itself: nothing in this process may apply them, and the
                    // studio is the only place that knows what a preset or a
                    // bypass means.
                    if let Some(chrome) = processor.editor_chrome() {
                        while let Some((kind, value, insert_id)) = chrome.take_action() {
                            chrome_actions.push((instance_id.clone(), kind, value, insert_id));
                        }
                    }
                    // VST2 editors repaint and animate only while the host
                    // calls effEditIdle; VST3 and CLAP run their own timers and
                    // this is a no-op for them. UI thread, never the producer.
                    processor.view_idle();
                    // Safe mode: no extra per-editor pump here — the main
                    // `pump_messages` below drains the whole thread queue.
                    if !platform::editor_safe_mode() {
                        if let Some(host_hwnd) =
                            registry.get(&instance_id).map(|state| state.host_hwnd)
                        {
                            platform::pump_editor_messages(host_hwnd);
                        }
                    }
                }
            }
            let au_refresh_targets: Vec<(String, Arc<AuHostProcessor>)> = au_processors
                .lock()
                .ok()
                .map(|processors| {
                    registry
                        .keys()
                        .filter_map(|id| {
                            processors
                                .get(id)
                                .cloned()
                                .map(|processor| (id.clone(), processor))
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (instance_id, processor) in au_refresh_targets {
                if processor.take_editor_user_close() {
                    user_closed.push(instance_id);
                }
            }
            user_closed
        });
        for (instance_id, kind, value, tab_id) in chrome_actions {
            let Some(action) = EditorChromeCommand::from_wire(kind, value, tab_id) else {
                eprintln!("[PluginEditorChrome] unknown action kind={kind} value={value}");
                continue;
            };
            let _ = ipc::write_frame(
                &mut out,
                &HostEvent::EditorChromeAction {
                    plugin_instance_id: instance_id,
                    action,
                },
            );
        }
        for instance_id in user_closed_editors {
            eprintln!(
                "[PluginEditor] user closed host-owned editor window instance={instance_id} (instance stays active)"
            );
            registry.remove(&instance_id);
            pending_resizes.remove(&instance_id);
            pending_editor_attaches.remove(&instance_id);
            // Detach the editor view (not the audio instance). Blocking lock,
            // matching the CloseEditor command handler — this is a rare one-shot
            // on user close, never a per-tick operation, so it cannot spin the
            // pump on the DSP mutex.
            if let Some(au) = au_instance(&au_processors, &instance_id) {
                au.close_editor();
            } else {
                preview.lock().editor_detach_for_instance(&instance_id);
            }
            let _ = ipc::write_frame(
                &mut out,
                &HostEvent::EditorClosed {
                    plugin_instance_id: instance_id,
                },
            );
        }
        timed_section!("resize_poll", {
            let resizes = preview
                .try_lock_for(Duration::from_millis(2))
                .map(|engine| engine.poll_pending_editor_resizes())
                .unwrap_or_default();
            for (instance_id, width, height) in resizes {
                eprintln!(
                    "[PluginEditor] top window resize notify instance={instance_id} content={width}x{height}"
                );
                let _ = ipc::write_frame(
                    &mut out,
                    &HostEvent::EditorContentResize {
                        plugin_instance_id: instance_id,
                        width,
                        height,
                        dpi: platform::system_dpi(),
                    },
                );
            }
        });
        let now = Instant::now();
        timed_section!("delayed_redraw", {
            delayed_redraws.retain(|entry| {
                if now >= entry.deadline {
                    let processor = preview
                        .try_lock_for(Duration::from_millis(2))
                        .and_then(|engine| engine.clone_processor_for(&entry.instance_id));
                    let Some(processor) = processor else {
                        // Lock busy — keep the entry and retry next tick.
                        return true;
                    };
                    if let Some((width, height)) = entry.second_resize {
                        eprintln!(
                            "[PluginEditorLifecycle] second resize instance={} size={}x{}",
                            entry.instance_id, width, height
                        );
                        eprintln!("[editor-size] delayed resize = {}x{}", width, height);
                        processor.view_set_size(width as i32, height as i32);
                    }
                    if !platform::editor_safe_mode() {
                        if let Some(host_hwnd) = registry
                            .get(&entry.instance_id)
                            .map(|state| state.host_hwnd)
                        {
                            platform::pump_editor_messages(host_hwnd);
                        }
                    }
                    false
                } else {
                    true
                }
            });
        });
        platform::set_editor_roots(registry.values().map(|state| state.host_hwnd).collect());
        let dispatched = platform::pump_messages();

        // Reported after the pumps, not before them: both pumps claim the key
        // into the counter this drains, so draining first meant a press was
        // held until the *next* iteration — and with an editor open the loop
        // then parks in `wait_for_input` first, so a single Space with no
        // further input waited out that timeout before the transport heard it.
        //
        // The main app owns what play/pause means; this only reports that the
        // key was pressed somewhere the main app's window could not see it.
        timed_section!("transport_key_poll", {
            for time_ms in platform::take_transport_toggle_requests() {
                if platform::plugin_debug() {
                    eprintln!(
                        "[Keyboard] host reporting transport key time={}",
                        time_ms
                            .map(|t| t.to_string())
                            .unwrap_or_else(|| "-".to_string())
                    );
                }
                let _ = ipc::write_frame(
                    &mut out,
                    &HostEvent::TransportToggleRequested {
                        source: "editor".to_string(),
                        time_ms,
                    },
                );
            }
        });
        // Freeze watchdog tiers (spec item 10):
        //  >50ms   name the slow section,
        //  >1000ms dump the window/thread snapshot,
        //  >3000ms notify the main app so it can surface "not responding"
        //          (the wrapper + close path live in the main process, so the
        //          user can always close a wedged editor).
        let gap_ms = last_pump_done.elapsed().as_millis() as u64;
        if !registry.is_empty() {
            static PUMP_GAP_RATE: platform::LogRate = platform::LogRate::new(1);
            if gap_ms > 50 && PUMP_GAP_RATE.allow() {
                eprintln!(
                    "[PluginUIThread] pump gap ms={gap_ms} suspected_block={slowest_section} \
                     section_ms={slowest_section_ms} dispatched={dispatched}"
                );
            }
            if gap_ms > 1000 {
                platform::plugin_editor_snapshot("pump_gap");
            }
            if gap_ms > 3000 {
                eprintln!(
                    "[PluginUIThread] editor not responding gap_ms={gap_ms} notifying_main_app=true"
                );
                for instance_id in registry.keys() {
                    let _ = ipc::write_frame(
                        &mut out,
                        &HostEvent::EditorUnresponsive {
                            plugin_instance_id: instance_id.clone(),
                            gap_ms,
                        },
                    );
                }
            }
        }
        last_pump_done = Instant::now();

        // Stage 3 block production runs on the dedicated `plugin-host-audio`
        // thread (see `run_audio_producer`) — not here — so the engine's block
        // rate is met instead of throttled to this ~120 Hz idle loop.

        let tick = IDLE_TICK.fetch_add(1, Ordering::Relaxed);

        // Republish every loaded plug-in's latency on the idle loop.
        //
        // The producer publishes it too, but only every 64 blocks it actually
        // processes, so a plug-in prepared on a stopped transport has never
        // reported one and its editor reads 0 ms. This loop runs whether or not
        // audio is flowing, which is exactly when the number is asked for. Cheap
        // and throttled: one `getLatencySamples` per instance per ~4 s, and the
        // store is skipped when nothing moved.
        if tick.is_multiple_of(480) {
            let mapped: Vec<String> = region_slots
                .lock()
                .map(|slots| slots.keys().cloned().collect())
                .unwrap_or_default();
            for instance_id in mapped {
                let Some(processor) = preview.lock().clone_processor_for(&instance_id) else {
                    continue;
                };
                let latency = processor.get_latency_samples().max(0) as u32;
                let Ok(slots) = region_slots.lock() else {
                    break;
                };
                let Some(region) = slots.get(&instance_id) else {
                    continue;
                };
                let bridge = region.bridge();
                if bridge.latency_samples.load(Ordering::Relaxed) == latency {
                    continue;
                }
                bridge.latency_samples.store(latency, Ordering::Relaxed);
                eprintln!("[plugin-host-dsp] latency instance={instance_id} samples={latency}");
            }
        }

        if platform::plugin_debug()
            && !platform::editor_safe_mode()
            && !registry.is_empty()
            && tick.is_multiple_of(120)
        {
            // Track plugin-created child/popup/dialog windows. Throttled and
            // fully disabled outside explicit plug-in diagnostics.
            let roots: Vec<u64> = registry.values().map(|state| state.host_hwnd).collect();
            platform::log_window_tree_changes(&roots, &mut window_tree);
        }
        if platform::plugin_debug() && tick.is_multiple_of(600) {
            eprintln!(
                "[PluginUIThread] loop alive editor_count={}",
                registry.len()
            );
            eprintln!("[plugin-host-ui-thread] message_loop_running=true");
            eprintln!("[plugin-host-ui-thread] editor_count={}", registry.len());
            eprintln!("[plugin-host-ui-thread] idle_tick={tick}");
            if let Ok(slots) = region_slots.lock() {
                eprintln!(
                    "[plugin-host-bridge] shared_audio mapped_regions={}",
                    slots.len()
                );
                for (instance_id, region) in slots.iter() {
                    let bridge = region.bridge();
                    let (peak_l, peak_r) = bridge.meters();
                    eprintln!(
                        "[plugin-host-bridge] instance={instance_id} os={} request_seq={} done_seq={} xruns={} producer_deadline_misses={} process_us={} max_process_us={} dsp_output={} peak_l={peak_l:.3} peak_r={peak_r:.3}",
                        std::env::consts::OS,
                        bridge.request_seq.load(Ordering::Relaxed),
                        bridge.done_seq.load(Ordering::Relaxed),
                        bridge.xrun_count.load(Ordering::Relaxed),
                        bridge.producer_deadline_misses.load(Ordering::Relaxed),
                        bridge.last_process_micros.load(Ordering::Relaxed),
                        bridge.max_process_micros.load(Ordering::Relaxed),
                        if bridge.dsp_output_ready() {
                            "ready"
                        } else {
                            "pending"
                        }
                    );
                }
            }
        }

        // 3. Wait for input instead of busy-polling (spec item 3): the loop
        //    idles in MsgWaitForMultipleObjectsEx and wakes immediately on any
        //    queued message or a wake_ui_thread kick from the stdin reader.
        //    With editors open the timeout keeps the old ~120 Hz refresh
        //    cadence; idle it stretches to 50ms. CPU is ~0% when nothing
        //    happens either way.
        let (wait_ms, wait_mode): (u32, &'static str) = if registry.is_empty() {
            (50, "idle_msgwait_50ms")
        } else {
            (8, "editor_msgwait_8ms")
        };
        if platform::plugin_debug() && wait_mode != last_wait_mode {
            eprintln!("[PluginUIThread] idle wait mode={wait_mode}");
        }
        last_wait_mode = wait_mode;
        let woke_on_input = platform::wait_for_input(wait_ms);
        // Spin watchdog (spec item 3): repeated "input available" wakes that
        // then dispatch nothing means the queue is being signalled without
        // producing messages — the loop would spin at 100% CPU. Name it.
        if woke_on_input && dispatched == 0 {
            spin_iterations += 1;
            if spin_iterations >= 200 {
                eprintln!(
                    "[PluginUIThread] spin warning iterations={spin_iterations} messages=0 \
                     duration={:?}",
                    spin_window_start.elapsed()
                );
                spin_iterations = 0;
                spin_window_start = Instant::now();
            }
        } else {
            spin_iterations = 0;
            spin_window_start = Instant::now();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    cmd: HostCommand,
    registry: &mut Registry,
    loaded: &mut LoadedRegistry,
    pending_plugin_loads: &mut HashMap<String, PendingPluginLoad>,
    load_result_tx: &crossbeam_channel::Sender<PluginLoadResult>,
    _pending_prepare: &mut PendingPrepareRegistry,
    delayed_redraws: &mut Vec<DelayedGpuRedraw>,
    pending_resizes: &mut HashMap<String, (u32, u32, u32)>,
    pending_editor_attaches: &mut HashMap<String, PendingEditorAttach>,
    attach_result_tx: &crossbeam_channel::Sender<EditorAttachResult>,
    preview: &SharedPluginHostPreview,
    preview_output_started: &mut bool,
    region_slots: &SharedAudioRegions,
    builtin_processors: &SharedBuiltinProcessors,
    au_processors: &SharedAuProcessors,
    out: &mut io::Stdout,
) {
    match cmd {
        HostCommand::Hello {
            protocol_version,
            main_hwnd,
            session_id,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                hlog!(
                    "[PluginHostEditor] protocol mismatch client={protocol_version} host={PROTOCOL_VERSION}"
                );
            }
            store_main_hwnd(main_hwnd);
            if let Some(session) = session_id.as_deref() {
                eprintln!("[PluginHost] ipc hello session_id={session}");
            }
        }
        HostCommand::Ping => {
            hlog!("[PluginHostEditor] Ping → Pong");
            let _ = ipc::write_frame(
                out,
                &HostEvent::Pong {
                    pid: std::process::id(),
                },
            );
        }
        HostCommand::LoadPlugin {
            plugin_instance_id,
            plugin_path,
            class_id,
            sample_rate,
            max_block_size,
            format,
        } => {
            // The engine labels every bridged insert with its real format; an
            // older frame without the field falls back to path detection.
            let module_format = format
                .as_deref()
                .and_then(DirectAudio::PluginModuleFormat::from_label)
                .unwrap_or_else(|| DirectAudio::PluginModuleFormat::detect(&plugin_path));
            hlog!(
                "[plugin-host] LoadPlugin instance={plugin_instance_id} path={plugin_path} class_id={class_id} sr={sample_rate} block={max_block_size} format={}",
                module_format.label()
            );
            if !std::path::Path::new(&plugin_path).exists() {
                let error = format!("plugin path not found: {plugin_path}");
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginLoadFailed {
                        plugin_instance_id,
                        error,
                    },
                );
                return;
            }
            let name = std::path::Path::new(&plugin_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("VST3 Plugin")
                .to_string();
            if loaded.contains_key(&plugin_instance_id) {
                eprintln!(
                    "[plugin-host] LoadPlugin instance={plugin_instance_id} already_loaded=true reuse=true"
                );
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginAlreadyLoaded {
                        plugin_instance_id,
                        name,
                    },
                );
                return;
            }
            if pending_plugin_loads.contains_key(&plugin_instance_id) {
                eprintln!(
                    "[plugin-host] LoadPlugin instance={plugin_instance_id} already_pending=true"
                );
                return;
            }
            let _ = ipc::write_frame(
                out,
                &HostEvent::PluginLoading {
                    plugin_instance_id: plugin_instance_id.clone(),
                },
            );
            // Default: create the controller on THIS persistent, message-pumped
            // main thread (not an ephemeral worker that dies). The editor attach
            // runs on this same thread, so a native editor's `attached()` send to
            // its controller-time window is a direct same-thread call — the fix
            // for the live AD2 `attached()` deadlock. See
            // `plugin_lifecycle_on_main_thread`.
            if plugin_lifecycle_on_main_thread() {
                let request_id = NEXT_PLUGIN_LOAD_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
                eprintln!(
                    "[plugin-host] LoadPlugin mode=main_thread instance={plugin_instance_id} thread_id={} name={name}",
                    platform::current_thread_id()
                );
                let started = Instant::now();
                let processor = DirectAudio::vst3_processor::Vst3RuntimeProcessor::new_with_format(
                    &plugin_path,
                    &class_id,
                    sample_rate,
                    module_format,
                );
                let elapsed = started.elapsed();
                let error = if processor.is_some() {
                    None
                } else {
                    Some(format!(
                        "Plugin failed to load. It may require a newer CPU instruction set or a missing runtime dependency. path={plugin_path}"
                    ))
                };
                finalize_plugin_load(
                    PluginLoadResult {
                        request_id,
                        plugin_instance_id,
                        plugin_path,
                        class_id,
                        name,
                        processor,
                        error,
                        elapsed,
                    },
                    sample_rate,
                    max_block_size,
                    loaded,
                    preview,
                    out,
                );
                return;
            }
            schedule_plugin_load(
                plugin_instance_id,
                plugin_path,
                class_id,
                name,
                sample_rate,
                max_block_size,
                module_format,
                pending_plugin_loads,
                load_result_tx,
            );
        }
        HostCommand::LoadBuiltinPlugin {
            plugin_instance_id,
            plugin_id,
            sample_rate,
            max_block_size,
            state_json,
        } => {
            let Some(stem) = SpherePluginHost::resolve_builtin_stem(&plugin_id) else {
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginLoadFailed {
                        plugin_instance_id,
                        error: format!("unknown built-in plugin: {plugin_id}"),
                    },
                );
                return;
            };
            let name = SpherePluginHost::builtin_display_name(stem)
                .unwrap_or(stem)
                .to_string();
            if loaded.contains_key(&plugin_instance_id) {
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginAlreadyLoaded {
                        plugin_instance_id,
                        name,
                    },
                );
                return;
            }
            // The catalog can list a built-in the host has no DSP for; fail the
            // load rather than publishing an instance that produces nothing.
            let Some(processor) =
                BuiltinHostProcessor::new(stem, sample_rate, state_json.as_deref())
            else {
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginLoadFailed {
                        plugin_instance_id,
                        error: format!("built-in DSP is not host-enabled yet: {stem}"),
                    },
                );
                return;
            };
            if let Ok(mut processors) = builtin_processors.lock() {
                Arc::make_mut(&mut processors)
                    .insert(plugin_instance_id.clone(), Arc::new(processor));
            }
            loaded.insert(
                plugin_instance_id.clone(),
                LoadedPlugin {
                    plugin_path: String::new(),
                    class_id: stem.to_string(),
                    name: name.clone(),
                    sample_rate,
                    max_block_size,
                    processing_ready: true,
                },
            );
            eprintln!(
                "[plugin-host-builtin] loaded instance={plugin_instance_id} plugin={stem} sr={sample_rate} block={max_block_size}"
            );
            let _ = ipc::write_frame(
                out,
                &HostEvent::PluginLoaded {
                    plugin_instance_id,
                    name,
                },
            );
        }
        HostCommand::LoadAudioUnit {
            plugin_instance_id,
            component_id,
            sample_rate,
            max_block_size,
            state_b64,
        } => {
            use base64::Engine as _;
            hlog!(
                "[plugin-host] LoadAudioUnit instance={plugin_instance_id} component={component_id} sr={sample_rate} block={max_block_size}"
            );
            if loaded.contains_key(&plugin_instance_id) {
                let name = loaded
                    .get(&plugin_instance_id)
                    .map(|plugin| plugin.name.clone())
                    .unwrap_or_else(|| component_id.clone());
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginAlreadyLoaded {
                        plugin_instance_id,
                        name,
                    },
                );
                return;
            }
            let _ = ipc::write_frame(
                out,
                &HostEvent::PluginLoading {
                    plugin_instance_id: plugin_instance_id.clone(),
                },
            );
            let state = state_b64.as_deref().map(|b64| {
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .unwrap_or_else(|error| {
                        eprintln!(
                            "[plugin-host-au] instance={plugin_instance_id} state base64 invalid: {error}"
                        );
                        Vec::new()
                    })
            });
            // Instantiation, initialization, and the state restore all happen
            // here on the IPC thread, before the instance is published to the
            // audio producer — the same race-free window the built-in load uses.
            let processor =
                AuHostProcessor::open(&component_id, sample_rate, max_block_size, state.as_deref());
            let processor = match processor {
                Ok(processor) => processor,
                Err(error) => {
                    eprintln!(
                        "[plugin-host-au] load FAILED instance={plugin_instance_id} component={component_id} error={error}"
                    );
                    let _ = ipc::write_frame(
                        out,
                        &HostEvent::PluginLoadFailed {
                            plugin_instance_id,
                            error,
                        },
                    );
                    return;
                }
            };
            // The component id is the only name the host has; the catalog owns
            // the display name and the app already knows it.
            let name = component_id.clone();
            eprintln!(
                "[plugin-host-au] loaded instance={plugin_instance_id} component={component_id} \
                 out_channels={} midi={} instrument={} params={}",
                processor.output_channels(),
                processor.accepts_midi(),
                processor.is_instrument(),
                processor.parameters().len()
            );
            if let Ok(mut processors) = au_processors.lock() {
                Arc::make_mut(&mut processors)
                    .insert(plugin_instance_id.clone(), Arc::new(processor));
            }
            loaded.insert(
                plugin_instance_id.clone(),
                LoadedPlugin {
                    plugin_path: String::new(),
                    class_id: component_id,
                    name: name.clone(),
                    sample_rate,
                    max_block_size,
                    processing_ready: true,
                },
            );
            let _ = ipc::write_frame(
                out,
                &HostEvent::PluginLoaded {
                    plugin_instance_id,
                    name,
                },
            );
        }
        HostCommand::LoadBuiltinNamCapture {
            plugin_instance_id,
            name,
            json,
            stereo,
            full_rig,
        } => {
            // IPC thread: parse/build here (allocation-heavy, never on the
            // producer thread), then hand off through the loader's wait-free
            // cells; the producer adopts at its next block boundary.
            let processor = builtin_processors
                .lock()
                .ok()
                .and_then(|processors| processors.get(&plugin_instance_id).cloned());
            let result = match processor {
                Some(p) => match p.nam_loader.as_ref() {
                    Some(loader) => loader
                        .load_json(&json, name.clone(), p.sample_rate as f64, stereo, full_rig)
                        .map_err(|e| e.to_string()),
                    None => Err(format!(
                        "built-in DSP for {plugin_instance_id} has no capture stage"
                    )),
                },
                None => Err(format!(
                    "no built-in DSP instance loaded for {plugin_instance_id}"
                )),
            };
            let event = match result {
                Ok(info) => {
                    eprintln!(
                        "[plugin-host-nam] loaded instance={plugin_instance_id} name={} receptive_field={} stereo={stereo} full_rig={full_rig}",
                        info.name, info.receptive_field
                    );
                    HostEvent::BuiltinNamCaptureResult {
                        plugin_instance_id,
                        ok: true,
                        name: info.name,
                        error: None,
                        receptive_field: info.receptive_field as u64,
                        full_rig: info.full_rig,
                    }
                }
                Err(error) => {
                    eprintln!(
                        "[plugin-host-nam] load FAILED instance={plugin_instance_id} name={name} error={error}"
                    );
                    HostEvent::BuiltinNamCaptureResult {
                        plugin_instance_id,
                        ok: false,
                        name,
                        error: Some(error),
                        receptive_field: 0,
                        full_rig,
                    }
                }
            };
            let _ = ipc::write_frame(out, &event);
        }
        HostCommand::LoadBuiltinIr {
            plugin_instance_id,
            name,
            wav_b64,
        } => {
            use base64::Engine as _;
            // IPC thread: decode, resample and FFT here (allocation-heavy,
            // never on the producer thread), then hand off through the
            // loader's wait-free cells.
            let processor = builtin_processors
                .lock()
                .ok()
                .and_then(|processors| processors.get(&plugin_instance_id).cloned());
            let result = base64::engine::general_purpose::STANDARD
                .decode(wav_b64.as_bytes())
                .map_err(|e| format!("IR payload is not valid base64: {e}"))
                .and_then(|bytes| match processor {
                    Some(p) => match p.ir_loader.as_ref() {
                        Some(loader) => loader
                            .load_wav(&bytes, name.clone(), p.sample_rate as f64)
                            .map_err(|e| e.to_string()),
                        None => Err(format!(
                            "built-in DSP for {plugin_instance_id} has no cabinet stage"
                        )),
                    },
                    None => Err(format!(
                        "no built-in DSP instance loaded for {plugin_instance_id}"
                    )),
                });
            let event = match result {
                Ok(info) => {
                    eprintln!(
                        "[plugin-host-ir] loaded instance={plugin_instance_id} name={} frames={} stereo={} truncated={}",
                        info.name, info.frames, info.stereo, info.truncated
                    );
                    HostEvent::BuiltinIrResult {
                        plugin_instance_id,
                        ok: true,
                        name: info.name,
                        error: None,
                        frames: info.frames as u64,
                        latency_samples: info.latency_samples as u64,
                        stereo: info.stereo,
                        truncated: info.truncated,
                    }
                }
                Err(error) => {
                    eprintln!(
                        "[plugin-host-ir] load FAILED instance={plugin_instance_id} name={name} error={error}"
                    );
                    HostEvent::BuiltinIrResult {
                        plugin_instance_id,
                        ok: false,
                        name,
                        error: Some(error),
                        frames: 0,
                        latency_samples: 0,
                        stereo: false,
                        truncated: false,
                    }
                }
            };
            let _ = ipc::write_frame(out, &event);
        }
        HostCommand::OpenEditorWithParentHwnd {
            track_id,
            track_index,
            track_name,
            plugin_slot_id,
            plugin_instance_id,
            class_id,
            plugin_uid,
            plugin_display_name,
            owner_hwnd,
            parent_hwnd,
            width,
            height,
            dpi,
            ..
        } => {
            let display_title = plugin_display_name
                .clone()
                .unwrap_or_else(|| plugin_instance_id.clone());
            let requested_owner = owner_hwnd.unwrap_or(parent_hwnd);
            let editor_mode = editor_mode_name();
            let resolved_owner =
                resolve_editor_owner_hwnd(requested_owner, parent_hwnd, &editor_mode);
            eprintln!(
                "[editor-open] 05 ipc_received insert_id={} plugin={} thread_id={} requested_owner_hwnd=0x{:x} parent_hwnd=0x{parent_hwnd:x} resolved_owner_hwnd=0x{resolved_owner:x}",
                plugin_instance_id,
                display_title,
                platform::current_thread_id(),
                requested_owner,
            );
            eprintln!(
                "[OpenEditor/IPC] track_id={} track_index={} track_name={} slot_id={} instance_id={} class_id={} plugin_uid={} plugin={} requested_owner_hwnd=0x{:x} parent_hwnd=0x{parent_hwnd:x} resolved_owner_hwnd=0x{resolved_owner:x}",
                track_id.as_deref().unwrap_or("<unknown>"),
                track_index
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "<unknown>".to_string()),
                track_name.as_deref().unwrap_or("<unknown>"),
                plugin_slot_id.as_deref().unwrap_or("<unknown>"),
                plugin_instance_id,
                class_id,
                plugin_uid.as_deref().unwrap_or("<unknown>"),
                display_title,
                requested_owner,
            );
            platform::log_window_identity_chain("studio_main_hwnd", main_hwnd());
            platform::log_window_identity_chain("open_requested_owner", requested_owner);
            platform::log_window_identity_chain("open_parent_hwnd", parent_hwnd);
            platform::log_window_identity_chain("open_resolved_owner", resolved_owner);
            // Audio Units do not expose a VST3 IEditController/IPlugView. Their
            // custom Cocoa view belongs to the already-loaded AuHostProcessor,
            // so open it directly in this host process and report the same
            // host-owned editor lifecycle the Studio consumes for VST3.
            if let Some(au) = au_instance(au_processors, &plugin_instance_id) {
                match au.open_editor(&display_title, width, height) {
                    Some((handle, preferred_width, preferred_height)) => {
                        registry.insert(
                            plugin_instance_id.clone(),
                            EditorState {
                                plugin_instance_id: plugin_instance_id.clone(),
                                host_hwnd: handle,
                                owner_hwnd: resolved_owner,
                                display_title: display_title.clone(),
                                state: "Open",
                            },
                        );
                        eprintln!(
                            "[plugin-host-au] editor attached instance={plugin_instance_id} handle=0x{handle:x} size={preferred_width}x{preferred_height}"
                        );
                        let _ = ipc::write_frame(
                            out,
                            &HostEvent::EditorAttached {
                                plugin_instance_id,
                                result: 0,
                                preferred_width,
                                preferred_height,
                                resizable: false,
                                host_hwnd: handle,
                            },
                        );
                    }
                    None => emit_attach_failed(
                        out,
                        &plugin_instance_id,
                        "Audio Unit did not provide a Cocoa editor view",
                    ),
                }
                return;
            }
            schedule_unified_editor_attach(
                &plugin_instance_id,
                resolved_owner,
                &display_title,
                width,
                height,
                dpi,
                registry,
                loaded,
                delayed_redraws,
                pending_editor_attaches,
                attach_result_tx,
                preview,
                out,
            );
        }
        HostCommand::PrepareEditorView {
            plugin_instance_id, ..
        } => {
            eprintln!("[PluginHost] editor open requested id={plugin_instance_id}");
            if !preview.lock().has_instance(&plugin_instance_id) {
                emit_attach_failed(
                    out,
                    &plugin_instance_id,
                    "plugin not loaded — call LoadPlugin first",
                );
                return;
            }
            eprintln!("[plugin-host] OpenEditor uses existing instance={plugin_instance_id}");
            SpherePluginHost::plugin_host_preview::PluginHostPreviewEngine::verify_unified_runtime(
                &plugin_instance_id,
                &plugin_instance_id,
                &plugin_instance_id,
                &plugin_instance_id,
                &plugin_instance_id,
                &plugin_instance_id,
                &plugin_instance_id,
                &plugin_instance_id,
            );
            let (preferred_width, preferred_height) = preview
                .lock()
                .editor_content_size_for_instance(&plugin_instance_id);
            let _ = ipc::write_frame(
                out,
                &HostEvent::EditorPreferredSize {
                    plugin_instance_id,
                    width: preferred_width,
                    height: preferred_height,
                },
            );
        }
        HostCommand::ConfirmEditorContentReady {
            plugin_instance_id,
            parent_hwnd,
            width,
            height,
            dpi,
            ..
        } => {
            schedule_unified_editor_attach(
                &plugin_instance_id,
                parent_hwnd,
                &plugin_instance_id,
                width,
                height,
                dpi,
                registry,
                loaded,
                delayed_redraws,
                pending_editor_attaches,
                attach_result_tx,
                preview,
                out,
            );
        }
        HostCommand::ResizeEditor {
            plugin_instance_id,
            width,
            height,
            dpi,
        } => {
            // Coalesce into the pending map; applied by the UI loop with a
            // bounded try-lock (spec item 9: resizing must never block the
            // pump thread on the DSP/preview mutex). Interactive drags stream
            // many ResizeEditor commands — only the latest size matters.
            pending_resizes.insert(plugin_instance_id.clone(), (width, height, dpi));
            hlog!(
                "[PluginHostEditor] resize queued plugin_instance_id={plugin_instance_id} \
                 width={width} height={height} dpi={dpi}"
            );
        }
        HostCommand::SetEditorChrome {
            plugin_instance_id,
            title,
            active,
            cpu_label,
            latency_label,
            preset_label,
            presets,
            preset_index,
            tabs,
            active_tab,
            shows_insert_controls,
            palette,
        } => {
            // Drawn only where the editor window belongs to this process. On
            // Windows the studio draws the strip into its own window and every
            // call below is a no-op, which is what keeps this one command safe
            // to send from every platform.
            //
            // Whichever runtime owns this instance — an Audio Unit is hosted in
            // this process rather than through the module bridges, and both
            // answer the same question: which window is this editor in.
            //
            // A bounded try-lock on the module path, like the resize path:
            // chrome arrives on every studio poll and must never park the IPC
            // thread on the DSP mutex for a repaint.
            let chrome = match au_instance(au_processors, &plugin_instance_id) {
                Some(au) => au.editor_chrome(),
                None => preview
                    .try_lock_for(Duration::from_millis(2))
                    .and_then(|engine| engine.clone_processor_for(&plugin_instance_id))
                    .and_then(|processor| processor.editor_chrome()),
            };
            // No strip: this platform draws its chrome in the studio's own
            // window, or this instance has no editor open. Neither is an error.
            let Some(chrome) = chrome else {
                return;
            };
            chrome.begin();
            chrome.set_header(
                active,
                &preset_label,
                &cpu_label,
                &latency_label,
                &active_tab,
                shows_insert_controls,
            );
            for (index, name) in presets.iter().enumerate() {
                chrome.add_preset(name, preset_index == Some(index as u32));
            }
            for tab in &tabs {
                chrome.add_tab(&tab.insert_id, &tab.display_name, tab.insert_number);
            }
            chrome.set_palette(&[
                palette.strip_bg,
                palette.row_bg,
                palette.border,
                palette.control_bg,
                palette.control_hover,
                palette.control_pressed,
                palette.accent,
                palette.text_primary,
                palette.text_secondary,
                palette.text_faint,
            ]);
            chrome.commit(&title);
        }
        HostCommand::CloseEditor { plugin_instance_id } => {
            eprintln!("[PluginEditor] close requested plugin_id={plugin_instance_id}");
            registry.remove(&plugin_instance_id);
            pending_editor_attaches.remove(&plugin_instance_id);
            pending_resizes.remove(&plugin_instance_id);
            let au = au_instance(au_processors, &plugin_instance_id);
            if let Some(au) = au.as_ref() {
                au.close_editor();
            } else {
                preview
                    .lock()
                    .editor_detach_for_instance(&plugin_instance_id);
            }
            delayed_redraws.retain(|entry| entry.instance_id != plugin_instance_id);
            let still_active = au.is_some() || preview.lock().has_instance(&plugin_instance_id);
            eprintln!("[PluginEditor] detached editor only plugin_id={plugin_instance_id}");
            eprintln!(
                "[PluginRuntime] instance remains alive plugin_id={plugin_instance_id} active={still_active}"
            );
            eprintln!("[AudioGraph] node remains active plugin_id={plugin_instance_id}");
            eprintln!("[VSTi] midi route alive plugin_id={plugin_instance_id}");
            eprintln!("[VSTi] process active after editor close plugin_id={plugin_instance_id}");
            let _ = ipc::write_frame(out, &HostEvent::EditorClosed { plugin_instance_id });
        }
        HostCommand::PreviewNoteOn {
            plugin_instance_id,
            channel,
            pitch,
            velocity,
        } => {
            // Stage 1: the legacy separate-CPAL audition stream is OFF by
            // default. The MIDI is still queued to the VSTi so the eventual
            // shared-memory mix path (Stage 3) can pull its output, but we never
            // open a second device stream — log the honest pending state instead.
            if debug_audio_out_enabled() {
                if !*preview_output_started {
                    *preview_output_started = try_start_preview_output(preview);
                }
            } else if !*preview_output_started {
                *preview_output_started = true; // log once
                eprintln!(
                    "[plugin-host-midi] dsp_output=pending reason=main_mix_integration_pending \
                     (separate CPAL preview disabled; set FUTUREBOARD_PLUGIN_HOST_CPAL_PREVIEW=1 to audition)"
                );
            }
            preview
                .lock()
                .preview_note_on(&plugin_instance_id, channel, pitch, velocity);
        }
        HostCommand::PreviewNoteOff {
            plugin_instance_id,
            channel,
            pitch,
        } => {
            preview
                .lock()
                .preview_note_off(&plugin_instance_id, channel, pitch);
        }
        HostCommand::PreviewControlChange {
            plugin_instance_id,
            channel,
            controller,
            value,
        } => {
            preview
                .lock()
                .preview_control_change(&plugin_instance_id, channel, controller, value);
        }
        HostCommand::PreviewAllNotesOff { plugin_instance_id } => {
            preview.lock().preview_all_notes_off(&plugin_instance_id);
        }
        HostCommand::MidiPanic { plugin_instance_id } => {
            if let Some(au) = au_instance(au_processors, &plugin_instance_id) {
                au.midi_panic();
                return;
            }
            preview.lock().midi_panic(&plugin_instance_id);
        }
        HostCommand::UnloadPlugin { plugin_instance_id } => {
            eprintln!(
                "[PluginHost] unload requested id={plugin_instance_id} reason=user_removed_insert"
            );
            // Unload is the authoritative lifetime boundary even if a preceding
            // CloseEditor frame was lost or an attach was still in flight. Drop
            // every editor callback/resize/redraw reference before releasing the
            // processor so no late completion can touch the destroyed instance.
            registry.remove(&plugin_instance_id);
            pending_editor_attaches.remove(&plugin_instance_id);
            pending_resizes.remove(&plugin_instance_id);
            delayed_redraws.retain(|entry| entry.instance_id != plugin_instance_id);
            if let Some(au) = au_instance(au_processors, &plugin_instance_id) {
                au.close_editor();
            }
            preview.lock().unload_instance(&plugin_instance_id);
            loaded.remove(&plugin_instance_id);
            if let Ok(mut processors) = builtin_processors.lock() {
                Arc::make_mut(&mut processors).remove(&plugin_instance_id);
            }
            if let Ok(mut processors) = au_processors.lock() {
                // Silence the unit before it leaves the producer's snapshot, so
                // an instrument cannot strand a held note in the tail.
                if let Some(au) = processors.get(&plugin_instance_id) {
                    au.midi_panic();
                }
                Arc::make_mut(&mut processors).remove(&plugin_instance_id);
            }
            if let Ok(mut slots) = region_slots.lock() {
                Arc::make_mut(&mut slots).remove(&plugin_instance_id);
            }
            let instance_count = preview.lock().loaded_instance_ids().len();
            eprintln!(
                "[PluginHost] host shutdown deferred instance_count={instance_count} editor_count={}",
                registry.len()
            );
            hlog!(
                "[PluginHostEditor] unload plugin_instance_id={plugin_instance_id} released=true"
            );
            let _ = ipc::write_frame(out, &HostEvent::PluginUnloaded { plugin_instance_id });
        }
        HostCommand::ConfigureAudioBridge {
            sample_rate,
            max_block_size,
        } => {
            // Stage 1: the main engine owns sample rate / block size; follow it.
            let (sr, block) = preview.lock().configure(sample_rate, max_block_size);
            eprintln!(
                "[plugin-host-bridge] ConfigureAudioBridge engine_sr={sample_rate} engine_block={max_block_size} \
                 host_sr={sr} host_block={block} follows_engine=true"
            );
            let _ = ipc::write_frame(
                out,
                &HostEvent::AudioBridgeConfigured {
                    sample_rate: sr,
                    max_block_size: block,
                    follows_engine: true,
                },
            );
        }
        HostCommand::ProcessBlockShared { block_id, frames } => {
            // Stage 1 skeleton: the lock-free shared-memory audio/MIDI transport
            // is Stage 2/3. Acknowledge honestly — plugin DSP output is NOT yet
            // mixed into the main engine.
            let dsp_ready = preview.lock().dsp_ready();
            let dsp_output = if dsp_ready { "ready" } else { "pending" };
            eprintln!(
                "[plugin-host-bridge] ProcessBlockShared block_id={block_id} frames={frames} dsp_output={dsp_output}"
            );
            let _ = ipc::write_frame(
                out,
                &HostEvent::AudioBridgeStatus {
                    block_id,
                    dsp_output: dsp_output.to_string(),
                    latency_samples: 0,
                },
            );
        }
        HostCommand::AttachSharedAudio {
            name,
            bytes,
            plugin_instance_id,
        } => match SharedAudioRegion::open_named(&name) {
            Ok(region) => {
                let sr = region.bridge().sample_rate.load(Ordering::Relaxed);
                let block = region.bridge().max_block_size.load(Ordering::Relaxed);
                eprintln!(
                        "[plugin-host-bridge] AttachSharedAudio instance={plugin_instance_id} name={name} bytes={bytes} attached=true header_sr={sr} header_block={block}"
                    );
                region.bridge().set_dsp_output_ready(true);
                if let Ok(mut slots) = region_slots.lock() {
                    let key = if plugin_instance_id.is_empty() {
                        name.clone()
                    } else {
                        plugin_instance_id.clone()
                    };
                    Arc::make_mut(&mut slots).insert(key, Arc::new(region));
                }
                preview.lock().set_dsp_ready(true);
                log_host_audio_mode();
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::SharedAudioAttached {
                        attached: true,
                        name,
                        bytes,
                    },
                );
            }
            Err(error) => {
                eprintln!(
                        "[plugin-host-bridge] AttachSharedAudio name={name} attached=false error={error}"
                    );
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::SharedAudioAttached {
                        attached: false,
                        name,
                        bytes,
                    },
                );
            }
        },
        HostCommand::PrepareProcessing {
            plugin_instance_id,
            sample_rate,
            max_block_size,
            input_channels,
            output_channels,
        } => {
            eprintln!(
                "[plugin-bridge] PrepareProcessing instance={plugin_instance_id} sr={sample_rate} block={max_block_size}"
            );
            // An Audio Unit negotiated its format at open, so there is nothing
            // to prepare — report what it settled on rather than failing the
            // instance for not being in the VST3 preview engine.
            if let Some(au) = au_instance(au_processors, &plugin_instance_id) {
                let channels = au.output_channels().max(1);
                eprintln!(
                    "[plugin-host-au] prepared instance={plugin_instance_id} sr={sample_rate} block={max_block_size} outputs={channels}"
                );
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::ProcessingPrepared {
                        plugin_instance_id,
                        sample_rate,
                        max_block_size,
                        output_channels: channels,
                        // One main bus; multi-out AU is a later slice.
                        output_bus_channels: vec![channels],
                    },
                );
                let _ = input_channels;
                return;
            }
            let mut preview_guard = preview.lock();
            if !preview_guard.has_instance(&plugin_instance_id) {
                drop(preview_guard);
                emit_attach_failed(
                    out,
                    &plugin_instance_id,
                    "PrepareProcessing: instance not loaded",
                );
                return;
            }
            eprintln!(
                "[plugin-host] PrepareProcessing uses existing instance={plugin_instance_id}"
            );
            let (sr, block) = preview_guard.configure(sample_rate, max_block_size);
            preview_guard.set_dsp_ready(true);
            let actual_output_channels = preview_guard
                .main_audio_output_channel_count_for_instance(&plugin_instance_id)
                .unwrap_or(output_channels.max(1));
            let output_bus_channels = preview_guard
                .output_bus_channel_counts_for_instance(&plugin_instance_id)
                .unwrap_or_default();
            drop(preview_guard);
            if let Some(plugin) = loaded.get_mut(&plugin_instance_id) {
                plugin.processing_ready = true;
                plugin.sample_rate = sr;
                plugin.max_block_size = block;
            }
            eprintln!(
                "[plugin-host-dsp] prepared instance={plugin_instance_id} sr={sr} block={block} requestedOutputs={output_channels} outputs={actual_output_channels} output_bus_channels={output_bus_channels:?} same_instance=true"
            );
            // Publish the plug-in's latency now rather than waiting for the
            // producer's 64-block cadence. A plug-in that has been prepared but
            // never played has a latency to report, and the editor asks for it
            // the moment it opens -- a stopped transport should not read 0 ms
            // for a plug-in that delays by 60.
            let prepared_latency = preview
                .lock()
                .clone_processor_for(&plugin_instance_id)
                .map(|processor| processor.get_latency_samples().max(0));
            if let Some(latency) = prepared_latency {
                if let Ok(slots) = region_slots.lock() {
                    if let Some(region) = slots.get(&plugin_instance_id) {
                        region
                            .bridge()
                            .latency_samples
                            .store(latency as u32, Ordering::Relaxed);
                        eprintln!(
                            "[plugin-host-dsp] latency instance={plugin_instance_id} samples={latency}"
                        );
                    }
                }
            }
            let _ = ipc::write_frame(
                out,
                &HostEvent::ProcessingPrepared {
                    plugin_instance_id,
                    sample_rate: sr,
                    max_block_size: block,
                    output_channels: actual_output_channels,
                    output_bus_channels,
                },
            );
            let _ = input_channels;
        }
        HostCommand::GetPluginState { plugin_instance_id } => {
            use base64::Engine as _;
            // AU state is a single opaque blob (the unit's ClassInfo binary
            // plist), so it travels in `component_b64` with no controller half.
            if let Some(au) = au_instance(au_processors, &plugin_instance_id) {
                let state = au.state();
                let ok = state.is_some();
                let bytes = state.unwrap_or_default();
                eprintln!(
                    "[plugin-host-au] get_state instance={plugin_instance_id} ok={ok} bytes={}",
                    bytes.len()
                );
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginState {
                        plugin_instance_id,
                        ok,
                        component_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                        controller_b64: String::new(),
                    },
                );
                return;
            }
            let state = preview.lock().get_instance_state(&plugin_instance_id);
            let ok = state.is_some();
            let state = state.unwrap_or_default();
            eprintln!(
                "[plugin-host-state] get_state instance={plugin_instance_id} ok={ok} component_bytes={} controller_bytes={}",
                state.component.len(),
                state.controller.len()
            );
            let _ = ipc::write_frame(
                out,
                &HostEvent::PluginState {
                    plugin_instance_id,
                    ok,
                    component_b64: base64::engine::general_purpose::STANDARD
                        .encode(&state.component),
                    controller_b64: base64::engine::general_purpose::STANDARD
                        .encode(&state.controller),
                },
            );
        }
        HostCommand::GetPluginParameters { plugin_instance_id } => {
            if let Some(au) = au_instance(au_processors, &plugin_instance_id) {
                let parameters: Vec<ipc::HostPluginParameter> = au
                    .parameters()
                    .iter()
                    .map(|parameter| ipc::HostPluginParameter {
                        id: parameter.id,
                        title: parameter.title.clone(),
                        // Audio Units expose one name per parameter.
                        short_title: parameter.title.clone(),
                        unit: parameter.unit.clone(),
                        automatable: parameter.automatable,
                        hidden: parameter.hidden,
                        read_only: parameter.read_only,
                    })
                    .collect();
                eprintln!(
                    "[plugin-host-au] get_parameters instance={plugin_instance_id} count={}",
                    parameters.len()
                );
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginParameters {
                        plugin_instance_id,
                        ok: true,
                        parameters,
                    },
                );
                return;
            }
            let params = preview
                .lock()
                .list_parameters_for_instance(&plugin_instance_id);
            let ok = params.is_some();
            let parameters: Vec<ipc::HostPluginParameter> = params
                .unwrap_or_default()
                .into_iter()
                .map(|p| ipc::HostPluginParameter {
                    id: p.id,
                    title: p.title,
                    short_title: p.short_title,
                    unit: p.unit,
                    automatable: p.automatable,
                    hidden: p.hidden,
                    read_only: p.read_only,
                })
                .collect();
            eprintln!(
                "[plugin-host-params] get_parameters instance={plugin_instance_id} ok={ok} count={}",
                parameters.len()
            );
            let _ = ipc::write_frame(
                out,
                &HostEvent::PluginParameters {
                    plugin_instance_id,
                    ok,
                    parameters,
                },
            );
        }
        HostCommand::SetPluginState {
            plugin_instance_id,
            component_b64,
            controller_b64,
        } => {
            use base64::Engine as _;
            if let Some(au) = au_instance(au_processors, &plugin_instance_id) {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&component_b64)
                    .unwrap_or_default();
                let ok = !bytes.is_empty() && au.set_state(&bytes);
                eprintln!(
                    "[plugin-host-au] set_state instance={plugin_instance_id} bytes={} ok={ok}",
                    bytes.len()
                );
                let _ = ipc::write_frame(
                    out,
                    &HostEvent::PluginStateSet {
                        plugin_instance_id,
                        ok,
                    },
                );
                return;
            }
            let decode = |label: &str, b64: &str| -> Vec<u8> {
                base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .unwrap_or_else(|error| {
                        eprintln!(
                            "[plugin-host-state] set_state instance={plugin_instance_id} {label} base64 invalid: {error}"
                        );
                        Vec::new()
                    })
            };
            let state = DirectAudio::vst3_processor::Vst3PluginState {
                component: decode("component", &component_b64),
                controller: decode("controller", &controller_b64),
            };
            let ok = if state.is_empty() {
                eprintln!(
                    "[plugin-host-state] set_state instance={plugin_instance_id} empty state — skipped"
                );
                false
            } else {
                preview
                    .lock()
                    .set_instance_state(&plugin_instance_id, &state)
            };
            let _ = ipc::write_frame(
                out,
                &HostEvent::PluginStateSet {
                    plugin_instance_id,
                    ok,
                },
            );
        }
        HostCommand::Shutdown => {
            // Handled in run_ipc_loop before dispatch; unreachable here.
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn schedule_plugin_load(
    plugin_instance_id: String,
    plugin_path: String,
    class_id: String,
    name: String,
    sample_rate: u32,
    max_block_size: u32,
    module_format: DirectAudio::PluginModuleFormat,
    pending_plugin_loads: &mut HashMap<String, PendingPluginLoad>,
    load_result_tx: &crossbeam_channel::Sender<PluginLoadResult>,
) {
    let request_id = NEXT_PLUGIN_LOAD_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let started_at = Instant::now();
    eprintln!(
        "[PLUGIN LOAD REQUEST]\nrequest_id={request_id}\nplugin_class_id={class_id}\nplugin_name={name}\nvendor=(unknown)\nformat={}\npath={plugin_path}\nproject_track_id=(unknown)\nrequested_by=user\nthread_id={}\nui_thread_blocked = false",
        module_format.label(),
        platform::current_thread_id()
    );
    pending_plugin_loads.insert(
        plugin_instance_id.clone(),
        PendingPluginLoad {
            request_id,
            plugin_instance_id: plugin_instance_id.clone(),
            plugin_path: plugin_path.clone(),
            class_id: class_id.clone(),
            name: name.clone(),
            sample_rate,
            max_block_size,
            started_at,
            stage: "create_instance",
        },
    );
    let tx = load_result_tx.clone();
    std::thread::Builder::new()
        .name(format!("plugin-load-{request_id}"))
        .spawn(move || {
            let thread_id = platform::current_thread_id();
            let stage_started = Instant::now();
            eprintln!(
                "[PLUGIN LOAD STAGE]\nrequest_id={request_id}\nplugin_instance_id_optional={plugin_instance_id}\nplugin_name={name}\nstage=load_module\nstarted_at_ms=0\nended_at_ms=0\nduration_ms=0\nresult=begin\nthread_id={thread_id}\ntimeout_ms={}\nlock_held_names=none\nipc_responsive=true\nui_responsive=true",
                MODULE_LOAD_TIMEOUT.as_millis()
            );
            eprintln!(
                "[PLUGIN LOAD STAGE]\nrequest_id={request_id}\nplugin_instance_id_optional={plugin_instance_id}\nplugin_name={name}\nstage=create_instance\nstarted_at_ms=0\nended_at_ms=0\nduration_ms=0\nresult=begin\nthread_id={thread_id}\ntimeout_ms={}\nlock_held_names=none\nipc_responsive=true\nui_responsive=true",
                CREATE_INSTANCE_TIMEOUT.as_millis()
            );
            let processor = DirectAudio::vst3_processor::Vst3RuntimeProcessor::new_with_format(
                &plugin_path,
                &class_id,
                sample_rate,
                module_format,
            );
            let elapsed = stage_started.elapsed();
            let error = if processor.is_some() {
                None
            } else {
                Some(format!(
                    "Plugin failed to load. It may require a newer CPU instruction set or a missing runtime dependency. path={plugin_path}"
                ))
            };
            eprintln!(
                "[PLUGIN LOAD STAGE]\nrequest_id={request_id}\nplugin_instance_id_optional={plugin_instance_id}\nplugin_name={name}\nstage=initialize_component_controller\nstarted_at_ms=0\nended_at_ms={}\nduration_ms={}\nresult={}\nthread_id={thread_id}\ntimeout_ms={}\nlock_held_names=none\nipc_responsive=true\nui_responsive=true",
                elapsed.as_millis(),
                elapsed.as_millis(),
                if processor.is_some() { "ok" } else { "failed" },
                INITIALIZE_TIMEOUT.as_millis()
            );
            let _ = tx.send(PluginLoadResult {
                request_id,
                plugin_instance_id,
                plugin_path,
                class_id,
                name,
                processor,
                error,
                elapsed,
            });
        })
        .expect("spawn plugin load worker");
}

fn drain_plugin_load_results(
    loaded: &mut LoadedRegistry,
    pending_plugin_loads: &mut HashMap<String, PendingPluginLoad>,
    load_result_rx: &crossbeam_channel::Receiver<PluginLoadResult>,
    preview: &SharedPluginHostPreview,
    out: &mut io::Stdout,
) {
    while let Ok(result) = load_result_rx.try_recv() {
        let Some(pending) = pending_plugin_loads.remove(&result.plugin_instance_id) else {
            eprintln!(
                "[PLUGIN LOAD FAILURE]\nrequest_id={}\nplugin_name={}\nfailure_stage=late_result_after_timeout\nreason=load completed after timeout\ntimed_out = true\nplugin_instance_created={}\ncomponent_created={}\ncontroller_created={}\nroutes_created=false\nrollback_completed = true\napp_alive = true",
                result.request_id,
                result.name,
                result.processor.is_some(),
                result.processor.is_some(),
                result.processor.is_some()
            );
            drop(result.processor);
            continue;
        };
        if pending.request_id != result.request_id {
            drop(result.processor);
            continue;
        }
        finalize_plugin_load(
            result,
            pending.sample_rate,
            pending.max_block_size,
            loaded,
            preview,
            out,
        );
    }
}

/// Register a completed plugin load (insert into the engine + `loaded`) and emit
/// `PluginLoaded`/`PluginLoadFailed`. Shared by the inline (main-thread) load
/// path and the legacy worker-thread drain so both report identically.
fn finalize_plugin_load(
    result: PluginLoadResult,
    sample_rate: u32,
    max_block_size: u32,
    loaded: &mut LoadedRegistry,
    preview: &SharedPluginHostPreview,
    out: &mut io::Stdout,
) {
    let Some(processor) = result.processor else {
        let error = result
            .error
            .unwrap_or_else(|| "plugin load failed".to_string());
        eprintln!(
            "[PLUGIN LOAD FAILURE]\nrequest_id={}\nplugin_name={}\nfailure_stage=create_instance\nreason={error}\ntimed_out = false\nplugin_instance_created=false\ncomponent_created=false\ncontroller_created=false\nroutes_created=false\nrollback_completed = true\napp_alive = true",
            result.request_id, result.name
        );
        let _ = ipc::write_frame(
            out,
            &HostEvent::PluginLoadFailed {
                plugin_instance_id: result.plugin_instance_id,
                error,
            },
        );
        return;
    };
    let inserted = preview
        .lock()
        .insert_loaded_instance(&result.plugin_instance_id, processor);
    if !inserted {
        let error = "plugin instance already exists".to_string();
        let _ = ipc::write_frame(
            out,
            &HostEvent::PluginLoadFailed {
                plugin_instance_id: result.plugin_instance_id,
                error,
            },
        );
        return;
    }
    loaded.insert(
        result.plugin_instance_id.clone(),
        LoadedPlugin {
            plugin_path: result.plugin_path,
            class_id: result.class_id,
            name: result.name.clone(),
            sample_rate,
            max_block_size,
            processing_ready: false,
        },
    );
    eprintln!(
        "[PLUGIN LOAD READY]\nrequest_id={}\nplugin_instance_id={}\nplugin_name={}\nload_duration_ms={}\naudio_ready = true\neditor_created = false\nroute_ready = true\nmixer_channels_created=0\nselected_bus_mode=capability_detected\nui_responsive = true",
        result.request_id,
        result.plugin_instance_id,
        result.name,
        result.elapsed.as_millis()
    );
    let _ = ipc::write_frame(
        out,
        &HostEvent::PluginLoaded {
            plugin_instance_id: result.plugin_instance_id,
            name: result.name,
        },
    );
}

fn expire_plugin_load_requests(
    pending_plugin_loads: &mut HashMap<String, PendingPluginLoad>,
    _load_result_rx: &crossbeam_channel::Receiver<PluginLoadResult>,
    out: &mut io::Stdout,
) {
    let now = Instant::now();
    let timed_out: Vec<PendingPluginLoad> = pending_plugin_loads
        .values()
        .filter(|pending| {
            let timeout = match pending.stage {
                "load_module" => MODULE_LOAD_TIMEOUT,
                "create_instance" => CREATE_INSTANCE_TIMEOUT,
                "initializing" => INITIALIZE_TIMEOUT,
                _ => CREATE_INSTANCE_TIMEOUT,
            };
            now.duration_since(pending.started_at) >= timeout
        })
        .cloned()
        .collect();
    for pending in timed_out {
        pending_plugin_loads.remove(&pending.plugin_instance_id);
        let elapsed_ms = now.duration_since(pending.started_at).as_millis();
        let timeout_ms = match pending.stage {
            "load_module" => MODULE_LOAD_TIMEOUT.as_millis(),
            "create_instance" => CREATE_INSTANCE_TIMEOUT.as_millis(),
            "initializing" => INITIALIZE_TIMEOUT.as_millis(),
            _ => CREATE_INSTANCE_TIMEOUT.as_millis(),
        };
        eprintln!(
            "[PLUGIN LOAD HANG WATCHDOG]\nrequest_id={}\nplugin_name={}\ncurrent_stage={}\nelapsed_ms={elapsed_ms}\ntimeout_ms={timeout_ms}\nthread_id={}\nlast_progress_ms=0\nui_thread_responsive=true\nipc_thread_responsive=true\naudio_thread_responsive=true\nheld_locks=none\nlast_vst3_call={}",
            pending.request_id,
            pending.name,
            pending.stage,
            platform::current_thread_id(),
            pending.stage
        );
        eprintln!(
            "[PLUGIN LOAD STAGE]\nrequest_id={}\nplugin_instance_id_optional={}\nplugin_name={}\nstage={}\nstarted_at_ms=0\nended_at_ms={elapsed_ms}\nduration_ms={elapsed_ms}\nresult=timed_out\nthread_id={}\ntimeout_ms={timeout_ms}\nlock_held_names=none\nipc_responsive=true\nui_responsive=true\nplugin_class_id={}\npath={}",
            pending.request_id,
            pending.plugin_instance_id,
            pending.name,
            pending.stage,
            platform::current_thread_id(),
            pending.class_id,
            pending.plugin_path
        );
        eprintln!(
            "[PLUGIN LOAD FAILURE]\nrequest_id={}\nplugin_name={}\nfailure_stage={}\nreason=plugin load timed out\ntimed_out = true\nplugin_instance_created=false\ncomponent_created=false\ncontroller_created=false\nroutes_created=false\nrollback_completed = true\napp_alive = true",
            pending.request_id, pending.name, pending.stage
        );
        let _ = ipc::write_frame(
            out,
            &HostEvent::PluginLoadFailed {
                plugin_instance_id: pending.plugin_instance_id,
                error: format!(
                    "Plugin load timed out during {} after {elapsed_ms}ms",
                    pending.stage
                ),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
/// Whether the whole plugin UI lifecycle — controller creation (LoadPlugin) AND
/// editor attach (createView / `attached()` / onSize) — runs INLINE on the host's
/// single, persistent, message-pumped main STA thread, instead of on ephemeral
/// per-request worker threads. Default = MAIN thread.
///
/// Live diagnosis (AD2, a NATIVE VST3 editor): `attached()` hard-blocks at the
/// very start with ZERO child windows created, and works in other DAWs. Root
/// cause = thread affinity. The controller was created on a `plugin-load-N`
/// worker that immediately DIED; native editors routinely `SendMessage()` from
/// `attached()` to a hidden window the plug-in created at controller-construction
/// time. With the controller's thread dead, that synchronous send blocks forever
/// (a dead thread never pumps). Every real host (and the VST3 SDK `editorhost`
/// sample) creates the controller and attaches the editor on the SAME living UI
/// thread, so the send is a direct same-thread call. Keeping the whole lifecycle
/// on the host main thread reproduces that. The audio `process()` stays on the
/// separate producer thread regardless. Set `FUTUREBOARD_PLUGIN_HOST_WORKER_THREADS=1`
/// to restore the legacy split-worker behavior for A/B testing.
fn plugin_lifecycle_on_main_thread() -> bool {
    std::env::var_os("FUTUREBOARD_PLUGIN_HOST_WORKER_THREADS").is_none()
}

fn editor_mode_name() -> String {
    std::env::var("FUTUREBOARD_PLUGIN_EDITOR_MODE").unwrap_or_else(|_| "detached".to_string())
}

fn editor_mode_prefers_async_attach(mode: &str) -> bool {
    let mode = mode.trim().to_ascii_lowercase();
    !matches!(
        mode.as_str(),
        "" | "default" | "legacy" | "child" | "ws_child" | "embedded_child"
    )
}

fn resolve_editor_owner_hwnd(requested_owner: u64, parent_hwnd: u64, editor_mode: &str) -> u64 {
    if !editor_mode_prefers_async_attach(editor_mode) {
        return parent_hwnd;
    }

    // Detached/owned top-level modes: the owner is a read-only DPI/position/owner
    // reference for the host's OWN window — never a `SetParent` target. Studio sends
    // its live main-window HWND with EVERY open request (`requested_owner`/
    // `parent_hwnd`), so that is the current, correct handle. Prefer it as long as
    // it is a real window owned by the Studio (parent) process.
    //
    // Do NOT require the host's `main_hwnd()` copy: it is captured once at `Hello`
    // (spawn) time via the spawn config, frequently BEFORE the GPUI window exists,
    // so it is an unreliable 0. Requiring it regressed editor opening — every open
    // resolved owner=0 → `schedule_unified_editor_attach` rejected `parent_hwnd` as
    // "not a valid window" before attach ever ran.
    let parent_pid = parse_parent_pid();
    for candidate in [requested_owner, parent_hwnd, main_hwnd()] {
        if candidate == 0 || !platform::is_window(candidate) {
            continue;
        }
        // When the parent pid is known, only accept a window the parent owns — this
        // keeps the "don't parent/own under an arbitrary foreign HWND" guarantee
        // without depending on the stale stored handle.
        if let (Some(ppid), cpid) = (parent_pid, platform::window_process_id(candidate)) {
            if cpid != Some(ppid) {
                eprintln!(
                    "[PluginEditorOwner] skipping owner hwnd=0x{candidate:x} pid={cpid:?} expected_parent_pid={ppid}"
                );
                continue;
            }
        }
        if candidate != requested_owner {
            eprintln!(
                "[PluginEditorOwner] using owner hwnd=0x{candidate:x} (requested=0x{requested_owner:x})"
            );
        }
        return candidate;
    }

    eprintln!(
        "[PluginEditorOwner] no Studio-owned owner HWND available (requested=0x{requested_owner:x} parent=0x{parent_hwnd:x} stored=0x{:x}); proceeding without owner",
        main_hwnd()
    );
    0
}

#[allow(clippy::too_many_arguments)]
fn schedule_unified_editor_attach(
    plugin_instance_id: &str,
    parent_hwnd: u64,
    display_title: &str,
    width: u32,
    height: u32,
    dpi: u32,
    registry: &mut Registry,
    loaded: &LoadedRegistry,
    delayed_redraws: &mut Vec<DelayedGpuRedraw>,
    pending_editor_attaches: &mut HashMap<String, PendingEditorAttach>,
    attach_result_tx: &crossbeam_channel::Sender<EditorAttachResult>,
    preview: &SharedPluginHostPreview,
    out: &mut io::Stdout,
) {
    // The actual window ownership is decided by the C++ embed layer from
    // FUTUREBOARD_PLUGIN_EDITOR_MODE (default "detached" = host-owned top-level
    // owned popup). In detached mode `parent_hwnd` is the Studio main HWND used
    // as the Win32 owner, not a child parent and never SetParent.
    let editor_mode = editor_mode_name();
    eprintln!("[plugin-host] editor_mode={editor_mode} owner_hwnd=0x{parent_hwnd:x}");
    eprintln!("[VST3Editor] owner_hwnd=0x{parent_hwnd:x}");
    eprintln!("[PluginHost] open_editor requested instance_id={plugin_instance_id}");
    eprintln!(
        "[editor-open] host request plugin={} insert={} thread_id={} hwnd=0x{parent_hwnd:x} size={}x{}",
        display_title,
        plugin_instance_id,
        platform::current_thread_id(),
        width.max(1),
        height.max(1)
    );
    eprintln!("[plugin-host] OpenEditor uses existing instance={plugin_instance_id}");
    eprintln!(
        "[EDITOR OPEN START]\nplugin_instance_id={plugin_instance_id}\nparent_hwnd=0x{parent_hwnd:x}\nrequested_size={}x{}\ndpi={dpi}",
        width.max(1),
        height.max(1)
    );
    eprintln!(
        "[PLUGIN EDITOR OPEN REQUEST]\nplugin_instance_id={plugin_instance_id}\nplugin_name=(unknown)\nvendor=(unknown)\nformat=VST3\nthread={}\nhost_process_id={}\nexisting_editor={}\ncomponent_valid=(unknown)\ncontroller_valid=(unknown)\naudio_instance_alive=(checking)\nmixer_route_ready=(unknown)\nmultiout_enabled=(unknown)",
        platform::current_thread_id(),
        std::process::id(),
        registry.contains_key(plugin_instance_id)
    );
    if let Some(existing) = registry.get(plugin_instance_id) {
        eprintln!(
            "[PluginHost] editor_state instance_id={} state={}",
            existing.plugin_instance_id, existing.state
        );
        eprintln!("[plugin-host] editor already attached instance={plugin_instance_id}");
        if existing.plugin_instance_id != plugin_instance_id {
            eprintln!(
                "[VST3Editor] refusing to focus mismatched editor requested={plugin_instance_id} existing={}",
                existing.plugin_instance_id
            );
            return;
        }
        if platform::is_window(existing.host_hwnd) {
            eprintln!(
                "[PluginHost] focusing existing editor requested_instance_id={plugin_instance_id} existing_editor_instance_id={}",
                existing.plugin_instance_id
            );
            eprintln!(
                "[NativeEditorShell] title=\"{}\" owner_hwnd=0x{:x}",
                existing.display_title, existing.owner_hwnd
            );
            let mut focused = platform::focus_editor_window(existing.host_hwnd);
            if !focused {
                if let Some(mut processor) = preview.lock().clone_processor_for(plugin_instance_id)
                {
                    focused = processor.focus_editor();
                }
            }
            eprintln!("[NativeEditorShell] focus_existing result={focused}");
            eprintln!(
                "[editor-open] existing valid plugin={} insert={} hwnd=0x{:x} result={}",
                existing.display_title,
                plugin_instance_id,
                existing.host_hwnd,
                if focused { "focused" } else { "focus_failed" }
            );
            return;
        }
        eprintln!(
            "[editor-open] existing invalid plugin={} insert={} hwnd=0x{:x} result=recreate",
            existing.display_title, plugin_instance_id, existing.host_hwnd
        );
        registry.remove(plugin_instance_id);
    }
    if pending_editor_attaches.contains_key(plugin_instance_id) {
        eprintln!("[plugin-host] editor attach already pending instance={plugin_instance_id}");
        return;
    }
    if !loaded.contains_key(plugin_instance_id) {
        eprintln!("[PluginHost] instance lookup result=not_found instance_id={plugin_instance_id}");
        eprintln!(
            "[editor-open] 06 instance_lookup fail insert_id={plugin_instance_id} reason=not_in_loaded_registry"
        );
        eprintln!(
            "[editor-open] FAILED stage=instance_lookup reason=runtime_instance_not_loaded insert_id={plugin_instance_id}"
        );
        emit_attach_failed(
            out,
            plugin_instance_id,
            "plugin runtime instance is not loaded",
        );
        return;
    }
    if !preview.lock().has_instance(plugin_instance_id) {
        eprintln!("[PluginHost] instance lookup result=not_found instance_id={plugin_instance_id}");
        eprintln!(
            "[editor-open] 06 instance_lookup fail insert_id={plugin_instance_id} reason=not_in_preview_engine"
        );
        eprintln!(
            "[editor-open] FAILED stage=instance_lookup reason=plugin_not_loaded insert_id={plugin_instance_id}"
        );
        emit_attach_failed(
            out,
            plugin_instance_id,
            "plugin not loaded — call LoadPlugin first",
        );
        return;
    }
    eprintln!("[PluginHost] instance lookup result=found instance_id={plugin_instance_id}");
    eprintln!(
        "[editor-open] 06 instance_lookup ok insert_id={plugin_instance_id} plugin={display_title}"
    );
    // Host-owned / detached: `parent_hwnd` is only an optional owner/DPI
    // reference for the host's own top-level window — never a SetParent target.
    // On Linux the GTK editor ignores it entirely, so 0 (or a stale XID from a
    // pure-Wayland GPUI surface) must not block OpenEditorWithParentHwnd →
    // embed_editor → open_editor_linux. Legacy main-owned child embedding still
    // requires a real parent window.
    let detached_owner = editor_mode_prefers_async_attach(&editor_mode);
    if !detached_owner && !platform::is_window(parent_hwnd) {
        eprintln!(
            "[editor-open] shell/parent invalid plugin={display_title} insert={plugin_instance_id} hwnd=0x{parent_hwnd:x} result=failed"
        );
        emit_attach_failed(out, plugin_instance_id, "parent_hwnd is not a valid window");
        return;
    }
    if detached_owner && parent_hwnd != 0 && !platform::is_window(parent_hwnd) {
        eprintln!(
            "[editor-open] host-owned owner_ref 0x{parent_hwnd:x} not a live window; continuing with host-owned top-level (Linux GTK / Win32 detached)"
        );
    }
    if width < MIN_EDITOR_ATTACH_SIZE || height < MIN_EDITOR_ATTACH_SIZE {
        eprintln!(
            "[editor-open] shell size invalid plugin={display_title} insert={plugin_instance_id} hwnd=0x{parent_hwnd:x} size={}x{} result=failed",
            width, height
        );
        emit_attach_failed(
            out,
            plugin_instance_id,
            "editor content HWND is not ready or has invalid size",
        );
        return;
    }
    eprintln!(
        "[EDITOR HWND]\nplugin_instance_id={plugin_instance_id}\nparent_hwnd=0x{parent_hwnd:x}\nvalid=true"
    );
    eprintln!(
        "[editor-open] shell ready plugin={display_title} insert={plugin_instance_id} hwnd=0x{parent_hwnd:x} size={}x{} result=ok",
        width, height
    );
    let w = width.max(1) as i32;
    let h = height.max(1) as i32;
    eprintln!(
        "[PluginEditor] open requested while engine_state=Running transport_playing=unknown instance={plugin_instance_id}"
    );
    let processor = preview.lock().clone_processor_for(plugin_instance_id);
    let Some(processor) = processor else {
        eprintln!(
            "[editor-open] 07 controller_lookup fail insert_id={plugin_instance_id} reason=no_runtime_processor"
        );
        eprintln!(
            "[editor-open] FAILED stage=controller_lookup reason=processor_unavailable insert_id={plugin_instance_id}"
        );
        emit_attach_failed(
            out,
            plugin_instance_id,
            "plugin not loaded — call LoadPlugin first",
        );
        return;
    };
    eprintln!(
        "[editor-open] 07 controller_lookup ok insert_id={plugin_instance_id} plugin={display_title}"
    );
    let request_id = NEXT_EDITOR_ATTACH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);

    // The editor view MUST be created + `attached()` on the SAME thread that
    // created the controller. When `plugin_lifecycle_on_main_thread()` is true
    // (the default) LoadPlugin builds the controller inline on this persistent,
    // COM-STA, message-pumped main IPC thread; native VST3 editors routinely
    // `SendMessage()` from `attached()` to a hidden window the plug-in created at
    // controller-construction time, so attaching on any OTHER thread makes that
    // synchronous send target a window whose owning thread isn't servicing it →
    // `attached()` deadlocks and no editor ever appears. The worker-thread path
    // below is therefore only valid in the legacy split mode where the controller
    // ALSO lives on a worker (`FUTUREBOARD_PLUGIN_HOST_WORKER_THREADS=1`).
    //
    // Regression guard (2026-06-28): gating this on `editor_mode_prefers_async_attach`
    // routed the DEFAULT `detached` mode to the worker path, which hung in
    // `attached()` for every plug-in (load still worked, so the symptom was
    // "plugins load + audio works but no editor opens"). Do NOT re-add an
    // editor-mode condition here — the attach thread must track the LOAD thread,
    // not the window-ownership mode.
    if plugin_lifecycle_on_main_thread() {
        let thread_id = platform::current_thread_id();
        eprintln!(
            "[plugin-host] editor attach mode=main_thread instance={plugin_instance_id} thread_id={thread_id} message_loop=main_ipc_loop"
        );
        eprintln!(
            "[EDITOR THREAD CHECK]\nplugin_instance_id={plugin_instance_id}\nrequested_thread_id={thread_id}\ncurrent_thread_id={thread_id}\nis_audio_thread=false\nis_ui_thread=true\nis_plugin_host_thread=true\nmessage_loop_running=true\ncom_initialized=true\ndpi_awareness_context=per_monitor_v2"
        );
        processor.embed_set_instance_label(plugin_instance_id);
        // Real plug-in name for the "Loading Plugin <name>" shell overlay.
        processor.set_editor_title(display_title);
        eprintln!("[VST3Editor] createView instance_id={plugin_instance_id}");
        eprintln!("[plugin-editor] createView from existing controller (reuse loaded runtime)");
        eprintln!(
            "[editor-open] createView(editor) begin plugin={display_title} insert={plugin_instance_id} thread_id={thread_id}"
        );
        eprintln!(
            "[VST3 ATTACH VIEW]\nplugin_instance_id={plugin_instance_id}\nparent_hwnd=0x{parent_hwnd:x}\nsize={w}x{h}\nresult=begin"
        );
        let started = Instant::now();
        // The window belongs to the main app, which handed us its HWND. This
        // only creates the plug-in's view and attaches it there -- no shell, no
        // titlebar, no window procedure of ours in the way.
        let handle = processor
            .view_attach(parent_hwnd, (w, h))
            .map(|_| parent_hwnd);
        let elapsed = started.elapsed();
        let attach_hwnd = processor.embed_attach_hwnd();
        eprintln!(
            "[editor-open] attach {} plugin={} insert={} parent=0x{parent_hwnd:x} hwnd=0x{attach_hwnd:x} size={w}x{h} thread_id={thread_id} duration_ms={}",
            if handle.is_some() { "ok" } else { "failed" },
            display_title,
            plugin_instance_id,
            elapsed.as_millis()
        );
        let (preferred_width, preferred_height) = processor
            .view_size()
            .map(|(cw, ch)| (cw.max(1) as u32, ch.max(1) as u32))
            .unwrap_or((width.max(1), height.max(1)));
        let resizable = processor.editor_resizable().unwrap_or(true);
        let error = if handle.is_some() {
            None
        } else {
            Some("embed_editor failed on existing runtime instance".to_string())
        };
        eprintln!(
            "[VST3 ATTACH VIEW]\nplugin_instance_id={plugin_instance_id}\nparent_hwnd=0x{parent_hwnd:x}\nsize={w}x{h}\nresult={}\nduration_ms={}",
            if handle.is_some() { "ok" } else { "failed" },
            elapsed.as_millis()
        );
        // Emit EditorAttached / EditorAttachFailed and register the editor so the
        // main loop keeps pumping + refreshing it (shared with the worker drain).
        finalize_editor_attach(
            EditorAttachResult {
                request_id,
                plugin_instance_id: plugin_instance_id.to_string(),
                processor,
                handle,
                attach_hwnd,
                owner_hwnd: parent_hwnd,
                display_title: display_title.to_string(),
                preferred_width,
                preferred_height,
                resizable,
                error,
                elapsed,
            },
            registry,
            delayed_redraws,
            preview,
            out,
        );
        return;
    }

    pending_editor_attaches.insert(
        plugin_instance_id.to_string(),
        PendingEditorAttach {
            request_id,
            plugin_instance_id: plugin_instance_id.to_string(),
            parent_hwnd,
            display_title: display_title.to_string(),
            started_at: Instant::now(),
            stage: "attach_view",
            timeout_logged: false,
            processor: processor.clone(),
        },
    );
    eprintln!(
        "[EDITOR THREAD CHECK]\nplugin_instance_id={plugin_instance_id}\nrequested_thread_id={}\ncurrent_thread_id={}\nis_audio_thread=false\nis_ui_thread=false\nis_plugin_host_thread=true\nmessage_loop_running=true\ncom_initialized=true\ndpi_awareness_context=per_monitor_v2",
        platform::current_thread_id(),
        platform::current_thread_id()
    );
    eprintln!(
        "[VST3 EDITOR LIFECYCLE]\nplugin_instance_id={plugin_instance_id}\nstep=resolve_instance\nresult=ok\nduration_ms=0\nthread_id={}\npointer_valid=true\nerror_code=0",
        platform::current_thread_id()
    );
    let tx = attach_result_tx.clone();
    let instance = plugin_instance_id.to_string();
    let display_title = display_title.to_string();
    std::thread::Builder::new()
        .name(format!("plugin-editor-attach-{request_id}"))
        .spawn(move || {
            let started = Instant::now();
            let thread_id = platform::current_thread_id();
            let _ = platform::pump_messages();
            eprintln!(
                "[EDITOR THREAD CHECK]\nplugin_instance_id={instance}\nrequested_thread_id={thread_id}\ncurrent_thread_id={thread_id}\nis_audio_thread=false\nis_ui_thread=true\nis_plugin_host_thread=false\nmessage_loop_running=true\ncom_initialized=(initialized_by_embed_editor)\ndpi_awareness_context=(initialized_by_embed_editor)"
            );
            eprintln!(
                "[plugin-host-ui-thread] editor_attach_thread_ready=true thread_id={thread_id} message_loop_running=true"
            );
            processor.embed_set_instance_label(&instance);
            // Real plug-in name for the "Loading Plugin <name>" shell overlay.
            processor.set_editor_title(&display_title);
            eprintln!("[VST3Editor] createView instance_id={instance}");
            eprintln!("[plugin-editor] createView from existing controller (reuse loaded runtime)");
            eprintln!(
                "[VST3 CREATE VIEW]\nplugin_instance_id={instance}\nthread_id={thread_id}\nresult=begin"
            );
            eprintln!(
                "[VST3 EDITOR LIFECYCLE]\nplugin_instance_id={instance}\nstep=create_view_editor\nresult=begin\nduration_ms=0\nthread_id={thread_id}\npointer_valid=true\nerror_code=0"
            );
            eprintln!(
                "[VST3 ATTACH VIEW]\nplugin_instance_id={instance}\nparent_hwnd=0x{parent_hwnd:x}\nsize={}x{}\nresult=begin",
                w,
                h
            );
            let handle = processor
                .view_attach(parent_hwnd, (w, h))
                .map(|_| parent_hwnd);
            let elapsed = started.elapsed();
            let attach_hwnd = processor.embed_attach_hwnd();
            let (preferred_width, preferred_height) = processor
                .view_size()
                .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32))
                .unwrap_or((width.max(1), height.max(1)));
            let resizable = processor.editor_resizable().unwrap_or(true);
            let error = if handle.is_some() {
                None
            } else {
                Some("embed_editor failed on existing runtime instance".to_string())
            };
            eprintln!(
                "[VST3 EDITOR LIFECYCLE]\nplugin_instance_id={instance}\nstep=attach_view\nresult={}\nduration_ms={}\nthread_id={thread_id}\npointer_valid={}\nerror_code={}",
                if handle.is_some() { "ok" } else { "failed" },
                elapsed.as_millis(),
                handle.is_some(),
                if handle.is_some() { 0 } else { 1 }
            );
            eprintln!(
                "[VST3 ATTACH VIEW]\nplugin_instance_id={instance}\nparent_hwnd=0x{parent_hwnd:x}\nsize={}x{}\nresult={}\nduration_ms={}",
                w,
                h,
                if handle.is_some() { "ok" } else { "failed" },
                elapsed.as_millis()
            );
            eprintln!(
                "[VST3 SIZE VIEW]\nplugin_instance_id={instance}\npreferred_size={}x{}\nresizable={resizable}\nresult={}",
                preferred_width,
                preferred_height,
                if handle.is_some() { "ok" } else { "failed" }
            );
            let attached = handle.is_some();
            let _ = tx.send(EditorAttachResult {
                request_id,
                plugin_instance_id: instance,
                processor: processor.clone(),
                handle,
                attach_hwnd,
                owner_hwnd: parent_hwnd,
                display_title,
                preferred_width,
                preferred_height,
                resizable,
                error,
                elapsed,
            });
            if attached {
                loop {
                    if !processor.view_is_attached() {
                        break;
                    }
                    let _ = platform::pump_messages();
                    let _ = platform::wait_for_input(8);
                }
            }
        })
        .expect("spawn plugin editor attach worker");
    hlog!(
        "[PluginHostEditor] attach scheduled request_id={request_id} onSize=({width}x{height}) dpi={dpi}"
    );
}

fn drain_editor_attach_results(
    registry: &mut Registry,
    delayed_redraws: &mut Vec<DelayedGpuRedraw>,
    pending_editor_attaches: &mut HashMap<String, PendingEditorAttach>,
    attach_result_rx: &crossbeam_channel::Receiver<EditorAttachResult>,
    preview: &SharedPluginHostPreview,
    out: &mut io::Stdout,
) {
    while let Ok(result) = attach_result_rx.try_recv() {
        let Some(pending) = pending_editor_attaches.remove(&result.plugin_instance_id) else {
            if result.handle.is_some() {
                eprintln!(
                    "[EDITOR FAILURE SAFE EXIT]\nplugin_instance_id={}\nfailure_stage=late_attach_after_timeout\nplugin_audio_kept_alive = true\neditor_state = failed\napp_frozen = false",
                    result.plugin_instance_id
                );
                result.processor.view_detach();
            }
            continue;
        };
        if pending.request_id != result.request_id {
            if result.handle.is_some() {
                result.processor.view_detach();
            }
            continue;
        }
        finalize_editor_attach(result, registry, delayed_redraws, preview, out);
    }
}

/// Register a completed editor attach and emit `EditorAttached`/`EditorAttachFailed`
/// to the main app. Shared by the inline (main-thread) attach path and the
/// legacy worker-thread drain so both report identically. The `attach_hwnd`
/// goes into `registry` so the host main loop pumps + refreshes the editor.
fn finalize_editor_attach(
    result: EditorAttachResult,
    registry: &mut Registry,
    delayed_redraws: &mut Vec<DelayedGpuRedraw>,
    preview: &SharedPluginHostPreview,
    out: &mut io::Stdout,
) {
    if let Some(error) = result.error {
        eprintln!(
            "[editor-open] failed plugin={} insert={} stage=attach_view result={} thread_id={}",
            result.display_title,
            result.plugin_instance_id,
            error,
            platform::current_thread_id()
        );
        emit_attach_failed(out, &result.plugin_instance_id, &error);
        eprintln!(
            "[EDITOR FAILURE SAFE EXIT]\nplugin_instance_id={}\nfailure_stage=attach_view\nplugin_audio_kept_alive = true\neditor_state = failed\napp_frozen = false",
            result.plugin_instance_id
        );
        return;
    }
    let Some(handle) = result.handle else {
        emit_attach_failed(
            out,
            &result.plugin_instance_id,
            "embed_editor failed on existing runtime instance",
        );
        return;
    };
    let attach_hwnd = result.attach_hwnd;
    if attach_hwnd == 0 {
        eprintln!(
            "[PluginEditorHWND] WARNING attach_hwnd unavailable instance={} handle={handle}",
            result.plugin_instance_id
        );
    }
    let host_hwnd = if attach_hwnd != 0 {
        attach_hwnd
    } else {
        handle
    };
    registry.insert(
        result.plugin_instance_id.clone(),
        EditorState {
            plugin_instance_id: result.plugin_instance_id.clone(),
            host_hwnd,
            owner_hwnd: result.owner_hwnd,
            display_title: result.display_title.clone(),
            state: "Open",
        },
    );
    eprintln!(
        "[PluginHost] editor_state instance_id={} state=Open owner_hwnd=0x{:x} host_hwnd=0x{host_hwnd:x}",
        result.plugin_instance_id, result.owner_hwnd
    );
    eprintln!(
        "[VST3Editor] attached instance_id={} title=\"{}\"",
        result.plugin_instance_id, result.display_title
    );
    preview.lock().set_continuous_mode(true);
    eprintln!(
        "[PluginEditorLifecycle] initial resize instance={} size={}x{}",
        result.plugin_instance_id, result.preferred_width, result.preferred_height
    );
    eprintln!(
        "[editor-size] client rect = {}x{}",
        result.preferred_width, result.preferred_height
    );
    result.processor.view_set_size(
        result.preferred_width as i32,
        result.preferred_height as i32,
    );
    eprintln!(
        "[editor-open] resize {}x{} ok plugin={} insert={} hwnd=0x{:x} thread_id={}",
        result.preferred_width,
        result.preferred_height,
        result.display_title,
        result.plugin_instance_id,
        attach_hwnd,
        platform::current_thread_id()
    );
    if platform::editor_safe_mode() {
        eprintln!("[PluginEditorSafe] attach: skipped focus walk and attach-time pump");
    } else {
        platform::focus_plugin_editor_child(attach_hwnd);
        platform::pump_editor_messages(attach_hwnd);
    }
    platform::log_capture_on_open(attach_hwnd);
    platform::set_editor_roots(registry.values().map(|state| state.host_hwnd).collect());
    platform::plugin_editor_snapshot("editor_open");
    eprintln!(
        "[PluginEditorResize] instance={} canResize={} preferred={}x{}",
        result.plugin_instance_id,
        result.resizable,
        result.preferred_width,
        result.preferred_height
    );
    eprintln!(
        "[PluginEditor] open complete engine_state=Running transport_playing=unknown instance={}",
        result.plugin_instance_id
    );
    eprintln!(
        "[editor-open] ready plugin={} insert={} hwnd=0x{:x} result=ok thread_id={}",
        result.display_title,
        result.plugin_instance_id,
        attach_hwnd,
        platform::current_thread_id()
    );
    SpherePluginHost::plugin_host_preview::PluginHostPreviewEngine::verify_unified_runtime(
        &result.plugin_instance_id,
        &result.plugin_instance_id,
        &result.plugin_instance_id,
        &result.plugin_instance_id,
        &result.plugin_instance_id,
        &result.plugin_instance_id,
        &result.plugin_instance_id,
        &result.plugin_instance_id,
    );
    eprintln!(
        "[plugin-host] attached result=ok instance={} handle=0x{handle:x} unified=true elapsed_ms={}",
        result.plugin_instance_id,
        result.elapsed.as_millis()
    );
    eprintln!(
        "[EDITOR MESSAGE PUMP]\nplugin_instance_id={}\nhost_window_hwnd=0x{:x}\nthread_id={}\npump_active=true\nlast_message_time_ms=0\nblocked_wait_detected=false\nipc_responsive=true\nwindow_responsive=true",
        result.plugin_instance_id,
        attach_hwnd,
        platform::current_thread_id()
    );
    delayed_redraws.push(DelayedGpuRedraw {
        instance_id: result.plugin_instance_id.clone(),
        deadline: Instant::now() + Duration::from_millis(100),
        second_resize: Some((result.preferred_width, result.preferred_height)),
    });
    let _ = ipc::write_frame(
        out,
        &HostEvent::EditorAttached {
            plugin_instance_id: result.plugin_instance_id,
            result: 0,
            preferred_width: result.preferred_width,
            preferred_height: result.preferred_height,
            resizable: result.resizable,
            host_hwnd: attach_hwnd,
        },
    );
}

fn expire_editor_attach_requests(
    pending_editor_attaches: &mut HashMap<String, PendingEditorAttach>,
    _attach_result_rx: &crossbeam_channel::Receiver<EditorAttachResult>,
    preview: &SharedPluginHostPreview,
    out: &mut io::Stdout,
) {
    let now = Instant::now();
    let timed_out: Vec<PendingEditorAttach> = pending_editor_attaches
        .values()
        .filter(|pending| {
            let timeout = match pending.stage {
                "create_view_editor" => EDITOR_CREATE_TIMEOUT,
                _ => EDITOR_ATTACH_TIMEOUT,
            };
            now.duration_since(pending.started_at) >= timeout
        })
        .cloned()
        .collect();
    for pending in timed_out {
        let elapsed_ms = now.duration_since(pending.started_at).as_millis();
        let Some(pending_live) = pending_editor_attaches.get_mut(&pending.plugin_instance_id)
        else {
            continue;
        };
        if pending_live.timeout_logged {
            continue;
        }
        pending_live.timeout_logged = true;
        pending_live
            .processor
            .embed_set_waiting_stage(pending.stage);
        eprintln!(
            "[EDITOR HANG DETECTED]\nplugin_instance_id={}\nplugin_name=(unknown)\nstage={}\nelapsed_ms={elapsed_ms}\nui_thread_blocked=false\nipc_thread_blocked=false\naudio_thread_blocked=false\nlast_successful_step=resolve_instance\nlast_vst3_result=(pending)\nhost_process_alive=true",
            pending.plugin_instance_id, pending.stage
        );
        eprintln!(
            "[PluginHost] editor_state instance_id={} state=Failed title=\"{}\"",
            pending.plugin_instance_id, pending.display_title
        );
        eprintln!(
            "[EDITOR HANG WATCHDOG]\nplugin_instance_id={}\nstage={}\nelapsed_ms={elapsed_ms}\ntimeout_ms={}\nui_thread_responsive=true\nipc_thread_responsive=true\naudio_thread_responsive=true\nhost_process_alive=true",
            pending.plugin_instance_id,
            pending.stage,
            EDITOR_ATTACH_TIMEOUT.as_millis()
        );
        eprintln!(
            "[EDITOR MESSAGE PUMP]\nplugin_instance_id={}\nhost_window_hwnd=0x{:x}\nthread_id={}\npump_active=true\nlast_message_time_ms=0\nblocked_wait_detected=true\nipc_responsive=true\nwindow_responsive=false",
            pending.plugin_instance_id,
            pending.parent_hwnd,
            platform::current_thread_id()
        );
        let audio_alive = preview.lock().has_instance(&pending.plugin_instance_id);
        eprintln!(
            "[EDITOR WAITING SAFE STATE]\nplugin_instance_id={}\nfailure_stage={}\nplugin_audio_kept_alive = {}\neditor_state = waiting\napp_frozen = false\nloading_shell_alive = true",
            pending.plugin_instance_id, pending.stage, audio_alive
        );
        let _ = out;
    }
}

fn emit_attach_failed(out: &mut io::Stdout, plugin_instance_id: &str, error: &str) {
    let _ = ipc::write_frame(
        out,
        &HostEvent::EditorAttachFailed {
            plugin_instance_id: plugin_instance_id.to_string(),
            error: error.to_string(),
        },
    );
}

/// Self-test path (`--selftest`): prove that the host can create a real
/// content **child** HWND distinct from a top HWND, with the required Win32
/// styles, and (optionally) attach a plugin to it. Drives the acceptance logs
/// without needing the main app or a real plugin.
///
/// Set `FUTUREBOARD_SELFTEST_PLUGIN_PATH` + `FUTUREBOARD_SELFTEST_CLASS_ID` to
/// also exercise a real VST3 attach. Exit code 0 on success.
fn run_selftest() -> i32 {
    match platform::create_selftest_windows() {
        Some((top_hwnd, content_hwnd)) => {
            let content_is_child = content_hwnd != top_hwnd && content_hwnd != 0;
            eprintln!("[plugin-view] selected_host_mode=main_owned_window");
            eprintln!("[plugin-view] top_hwnd=0x{top_hwnd:x}");
            eprintln!("[plugin-view] content_hwnd=0x{content_hwnd:x}");
            eprintln!("[plugin-view] content_is_child={content_is_child}");
            eprintln!("[plugin-view] content_parent=0x{top_hwnd:x}");
            if content_hwnd == top_hwnd {
                eprintln!("[plugin-view] ERROR content_hwnd == top_hwnd — not attaching");
                platform::destroy_selftest_windows(top_hwnd, content_hwnd);
                return 1;
            }
            eprintln!("[plugin-view] content_hwnd != top_hwnd");

            let mut code = 0;
            if let (Ok(path), Ok(class_id)) = (
                std::env::var("FUTUREBOARD_SELFTEST_PLUGIN_PATH"),
                std::env::var("FUTUREBOARD_SELFTEST_CLASS_ID"),
            ) {
                let region = EmbedRegion {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                };
                match native_editor::attach_editor_into_parent(
                    content_hwnd,
                    &path,
                    &class_id,
                    region,
                ) {
                    Ok(handle) => {
                        eprintln!("[vst3-editor] attached begin parent=0x{content_hwnd:x}");
                        eprintln!("[vst3-editor] attached result=ok handle=0x{handle:x}");
                        native_editor::detach_editor(handle);
                    }
                    Err(err) => {
                        eprintln!("[vst3-editor] attached result=err {err}");
                        code = 1;
                    }
                }
            } else {
                eprintln!(
                    "[plugin-view] selftest: no FUTUREBOARD_SELFTEST_PLUGIN_PATH/CLASS_ID — \
                     HWND hierarchy only"
                );
            }

            platform::destroy_selftest_windows(top_hwnd, content_hwnd);
            code
        }
        None => {
            eprintln!("[plugin-view] selftest: window creation unavailable on this platform");
            // Not a failure on non-Windows — there is nothing to host there yet.
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Platform shims. Windows is the real implementation; other targets get no-op
// stubs so the binary still compiles and the IPC loop still runs.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use windows::core::BOOL;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::Foundation::{LPARAM, RECT, WAIT_OBJECT_0, WPARAM};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::Win32::System::Threading::{
        GetCurrentProcessId, GetCurrentThreadId, GetExitCodeProcess, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, GetCapture, GetFocus, GetKeyState, IsWindowEnabled, ReleaseCapture,
        SetFocus, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SPACE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, CallNextHookEx, ChildWindowFromPointEx, CreateWindowExW, DestroyWindow,
        DispatchMessageW, EnumChildWindows, EnumThreadWindows, GetAncestor, GetClassNameW,
        GetForegroundWindow, GetGUIThreadInfo, GetMessageW, GetParent, GetWindow,
        GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsChild,
        IsDialogMessageW, IsWindow, IsWindowVisible, MsgWaitForMultipleObjectsEx, PeekMessageW,
        PostThreadMessageW, SetForegroundWindow, SetWindowPos, SetWindowsHookExW, ShowWindow,
        TranslateMessage, UnhookWindowsHookEx, WindowFromPoint, CWP_ALL, CW_USEDEFAULT, GA_PARENT,
        GA_ROOT, GUITHREADINFO, GWLP_HWNDPARENT, GWL_EXSTYLE, GWL_STYLE, GW_CHILD, GW_OWNER,
        HWND_TOP, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, MWMO_INPUTAVAILABLE, PM_REMOVE,
        QS_ALLINPUT, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_SHOWNORMAL, WH_KEYBOARD_LL,
        WINDOW_EX_STYLE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
        WM_MOUSEMOVE, WM_NULL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
        WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    /// End-to-end plugin debug switch (`FUTUREBOARD_PLUGIN_DEBUG=1`), shared
    /// with the narrower view-debug flag.
    pub fn plugin_debug() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| {
            std::env::var_os("FUTUREBOARD_PLUGIN_DEBUG").is_some()
                || std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some()
        })
    }

    /// Plugin editor safe mode (`FUTUREBOARD_PLUGIN_EDITOR_SAFE=1`): disables
    /// window-tree polling, per-message verbose logs, re-entrant pumping inside
    /// attach/load handlers, and experimental focus hacks. Keeps only minimal
    /// diagnostics (loop alive, pump gap, spin warning, focus/capture summary
    /// on click, snapshot on editor open).
    pub fn editor_safe_mode() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_EDITOR_SAFE").is_some())
    }

    /// Coarse log rate limiter: allows at most `max` events per second.
    pub struct LogRate {
        window_start_ms: std::sync::atomic::AtomicU64,
        count: std::sync::atomic::AtomicU32,
        max_per_sec: u32,
    }

    impl LogRate {
        pub const fn new(max_per_sec: u32) -> Self {
            Self {
                window_start_ms: std::sync::atomic::AtomicU64::new(0),
                count: std::sync::atomic::AtomicU32::new(0),
                max_per_sec,
            }
        }

        pub fn allow(&self) -> bool {
            use std::sync::atomic::Ordering;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let start = self.window_start_ms.load(Ordering::Relaxed);
            if now.saturating_sub(start) >= 1000 {
                self.window_start_ms.store(now, Ordering::Relaxed);
                self.count.store(1, Ordering::Relaxed);
                return true;
            }
            self.count.fetch_add(1, Ordering::Relaxed) < self.max_per_sec
        }
    }

    /// Editor root HWNDs currently registered (registry mirror) — feeds the
    /// click-path diagnostic and the on-demand window snapshot without
    /// threading the registry through every pump call.
    static EDITOR_ROOTS: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

    pub fn set_editor_roots(roots: Vec<u64>) {
        if let Ok(mut guard) = EDITOR_ROOTS.lock() {
            if *guard != roots {
                *guard = roots;
            }
        }
    }

    fn editor_roots() -> Vec<u64> {
        EDITOR_ROOTS.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn class_name(hwnd: HWND) -> String {
        if hwnd.0.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 128];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        if len > 0 {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        }
    }

    fn window_title(hwnd: HWND) -> String {
        if hwnd.0.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 256];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if len > 0 {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        }
    }

    fn hwnd_from(handle: u64) -> HWND {
        HWND(handle as *mut core::ffi::c_void)
    }

    /// Focus HWND for the actual foreground UI thread. `GetFocus()` only reports
    /// the calling thread's focus, which is wrong for plug-ins that create a
    /// dedicated JUCE/CEF UI thread and caused Space in their text fields to be
    /// mistaken for a transport command.
    /// The window that would receive a key this thread just peeked.
    ///
    /// `GetFocus` answers for the *calling thread's* queue, which is exactly
    /// the queue the message came off. [`foreground_focus_window`] answers for
    /// whichever thread owns the foreground window — and when an editor is
    /// embedded under a window owned by the Studio process, that is Studio's
    /// GPUI thread, so the text-entry veto was reading the class of a window
    /// in another process and vetoing (or failing to veto) on it.
    fn keyboard_focus_window() -> HWND {
        unsafe {
            let focus = GetFocus();
            if !focus.is_invalid() {
                return focus;
            }
        }
        foreground_focus_window()
    }

    fn foreground_focus_window() -> HWND {
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.is_invalid() {
                return HWND::default();
            }
            let thread_id = GetWindowThreadProcessId(foreground, None);
            let mut info = GUITHREADINFO {
                cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            if thread_id != 0 && GetGUIThreadInfo(thread_id, &mut info).is_ok() {
                if !info.hwndFocus.is_invalid() {
                    return info.hwndFocus;
                }
            }
            foreground
        }
    }

    /// True if `hwnd` is a real Win32 dialog (class `#32770`).
    fn is_dialog_class(hwnd: HWND) -> bool {
        if hwnd.0.is_null() {
            return false;
        }
        let mut buf = [0u16; 16];
        let len = unsafe { GetClassNameW(hwnd, &mut buf) };
        len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "#32770"
    }

    /// Nearest dialog (`#32770`) in the parent chain of `hwnd`, if any.
    /// `IsDialogMessageW` must only run against real dialog windows — calling
    /// it with an arbitrary window as the "dialog" swallows Tab/arrow/Enter/
    /// Escape keystrokes destined for plugin editor controls.
    fn dialog_ancestor(hwnd: HWND) -> Option<HWND> {
        let mut cur = hwnd;
        let mut depth = 0;
        while !cur.0.is_null() && depth < 32 {
            if is_dialog_class(cur) {
                return Some(cur);
            }
            cur = unsafe { GetAncestor(cur, GA_PARENT) };
            depth += 1;
        }
        None
    }

    pub fn com_init() {
        // STA: VST3 editors require apartment-threaded COM (spec Part 9).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        install_transport_keyboard_hook();
    }

    /// Low-level keyboard hook so Space still reaches the host when a plug-in's
    /// own UI thread (CEF / JUCE / browser editors) owns the message queue.
    /// Only claims when *this process* owns the foreground window — the main
    /// Studio process keeps Space when the arrangement is focused.
    ///
    /// On a thread of its own, and that is not a detail.
    ///
    /// Windows calls a `WH_KEYBOARD_LL` proc on the thread that installed it,
    /// and every keystroke *system-wide* waits for that call to return. This
    /// hook used to live on the editor UI thread, which also loads plug-ins
    /// (30 s timeout), drains up to 512 messages a pass, and blocks on the DSP
    /// mutex. While that thread was busy, typing anywhere on the machine
    /// stalled — and once a call exceeded `LowLevelHooksTimeout` (300 ms by
    /// default) Windows silently dropped the hook, which a `OnceLock` never
    /// reinstalls. That is the "Space worked, then stopped working" shape.
    ///
    /// This thread does nothing but service the hook, so it is always ready to
    /// answer. It parks in `GetMessageW` and exits with the process.
    fn install_transport_keyboard_hook() {
        static HOOK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        HOOK.get_or_init(|| {
            let spawned = std::thread::Builder::new()
                .name("plugin-host-transport-hook".into())
                .spawn(|| unsafe {
                    let hook = match SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(transport_ll_keyboard_proc),
                        None,
                        0,
                    ) {
                        Ok(hook) if !hook.is_invalid() => hook,
                        Ok(_) | Err(_) => {
                            eprintln!(
                                "[PluginEditorInput] WARNING failed to install WH_KEYBOARD_LL transport hook"
                            );
                            return;
                        }
                    };
                    eprintln!("[PluginEditorInput] WH_KEYBOARD_LL transport hook installed");
                    // A hook thread must pump, or the proc is never called.
                    // Nothing posts to this queue, so the loop simply parks
                    // here for the life of the process.
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    let _ = UnhookWindowsHookEx(hook);
                });
            if spawned.is_err() {
                eprintln!(
                    "[PluginEditorInput] WARNING could not start the transport hook thread; \
                     Space still reaches the transport through the editor message pumps"
                );
            }
        });
    }

    /// When Space went down, so auto-repeat toggles nothing.
    ///
    /// A low-level hook is handed raw key transitions, not messages, so there is
    /// no `lParam` bit 30 to consult: every repeat arrives as another
    /// `WM_KEYDOWN` and, unfiltered, held Space toggled the transport thirty
    /// times a second.
    ///
    /// The paired key-up is not something this can *wait* for. The hook is
    /// system-wide, so it normally sees every release wherever focus went, but a
    /// release swallowed by a hook ahead of this one -- or arriving while this
    /// one was uninstalled by `LowLevelHooksTimeout` and reinstalled -- would
    /// otherwise leave Space stuck "down" and the transport key dead for good.
    /// So the down state expires: nobody holds Space for two seconds and means
    /// the next press to be the same press.
    static SPACE_DOWN_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// How long a `SPACE_DOWN_AT` with no matching release stays believed.
    const SPACE_HELD_MAX_MS: u64 = 2_000;

    unsafe extern "system" fn transport_ll_keyboard_proc(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        if code >= 0 {
            let msg = wparam.0 as u32;
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let injected = (kb.flags.0 & LLKHF_INJECTED.0) != 0;
            let space = !injected && kb.vkCode == VK_SPACE.0 as u32;
            if space && (msg == WM_KEYUP || msg == WM_SYSKEYUP) {
                SPACE_DOWN_AT.store(0, std::sync::atomic::Ordering::Relaxed);
            }
            if space && (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN) {
                let now = GetTickCount64();
                let down_at = SPACE_DOWN_AT.load(std::sync::atomic::Ordering::Relaxed);
                let repeated = down_at != 0 && now.saturating_sub(down_at) < SPACE_HELD_MAX_MS;
                SPACE_DOWN_AT.store(now.max(1), std::sync::atomic::Ordering::Relaxed);
                let held = |vk: VIRTUAL_KEY| GetAsyncKeyState(vk.0 as i32) < 0;
                let modified = held(VK_CONTROL) || held(VK_MENU) || held(VK_LWIN) || held(VK_RWIN);
                // Keyboard input belongs to whoever owns *focus*, which is not
                // the same question as who owns the foreground window. An editor
                // embedded into a Studio-owned window is a plug-in view whose
                // top-level ancestor belongs to the Studio process, so asking
                // "is the foreground window mine" answered no for exactly the
                // case with no other route to the transport, and Space in a
                // docked ARA editor went into the plug-in and stopped there.
                //
                // Asking about the focus window instead gets both halves right:
                // the plug-in view is this process's, so this claims the key;
                // the arrangement is the Studio's, so GPUI's own key bindings
                // keep it and this never fires. One owner per press, decided by
                // the same thing Windows uses to route the key.
                let focus = foreground_focus_window();
                let mut focus_pid = 0u32;
                if !focus.is_invalid() {
                    GetWindowThreadProcessId(focus, Some(&mut focus_pid));
                }
                let ours = focus_pid == GetCurrentProcessId();
                let text = !focus.is_invalid() && is_text_entry_class(&class_name(focus));
                let claimed = ours && !text && !modified && !repeated;
                keyboard_trace(
                    "WH_KEYBOARD_LL",
                    focus,
                    focus_pid,
                    kb.time,
                    repeated,
                    claimed,
                );
                if claimed {
                    claim_transport_toggle(Some(kb.time));
                    // Swallow so the plug-in does not also act on Space.
                    return windows::Win32::Foundation::LRESULT(1);
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    /// One line per transport-key decision, because none of this is visible from
    /// outside: the key is either seen or it is not, and it either belongs to a
    /// plug-in or to the DAW.
    ///
    /// Gated behind `FUTUREBOARD_KEY_DEBUG`, except the first press this process
    /// declines -- that line is what somebody reporting "Space does nothing"
    /// will already have.
    fn keyboard_trace(
        site: &str,
        focus: HWND,
        focus_pid: u32,
        time_ms: u32,
        repeated: bool,
        claimed: bool,
    ) {
        static FIRST_DECLINE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !key_debug()
            && !plugin_debug()
            && (claimed || FIRST_DECLINE.swap(true, std::sync::atomic::Ordering::Relaxed))
        {
            return;
        }
        unsafe {
            let foreground = GetForegroundWindow();
            let mut fg_pid = 0u32;
            if !foreground.is_invalid() {
                GetWindowThreadProcessId(foreground, Some(&mut fg_pid));
            }
            eprintln!(
                "[Keyboard] host site={site} focus=0x{:x} focus_class='{}' focus_pid={focus_pid} \
                 foreground=0x{:x} foreground_pid={fg_pid} self_pid={} time={time_ms} \
                 repeated={repeated} claimed={claimed}",
                focus.0 as u64,
                class_name(focus),
                foreground.0 as u64,
                GetCurrentProcessId(),
            );
        }
    }

    /// Whether the keyboard-routing trace is on (`FUTUREBOARD_KEY_DEBUG`).
    fn key_debug() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_KEY_DEBUG").is_some())
    }

    pub fn ensure_dpi_awareness() {
        unsafe {
            let ctx = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            eprintln!(
                "[PluginEditor] dpi_awareness_context=0x{:x} tid={}",
                ctx.0 as usize,
                GetCurrentThreadId()
            );
        }
    }

    pub fn system_dpi() -> u32 {
        unsafe {
            let dpi = GetDpiForSystem();
            if dpi == 0 {
                96
            } else {
                dpi
            }
        }
    }

    pub fn com_uninit() {
        unsafe { CoUninitialize() };
    }

    pub fn current_thread_id() -> u64 {
        unsafe { GetCurrentThreadId() as u64 }
    }

    pub fn is_process_alive(pid: u32) -> bool {
        const STILL_ACTIVE: u32 = 259;
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut code = 0u32;
            let alive = if GetExitCodeProcess(handle, &mut code).is_ok() {
                code == STILL_ACTIVE
            } else {
                false
            };
            let _ = CloseHandle(handle);
            alive
        }
    }

    pub fn is_window(handle: u64) -> bool {
        if handle == 0 {
            return false;
        }
        unsafe { IsWindow(Some(hwnd_from(handle))).as_bool() }
    }

    pub fn window_process_id(handle: u64) -> Option<u32> {
        if handle == 0 {
            return None;
        }
        unsafe {
            let hwnd = hwnd_from(handle);
            if !IsWindow(Some(hwnd)).as_bool() {
                return None;
            }
            let mut pid = 0u32;
            let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 {
                None
            } else {
                Some(pid)
            }
        }
    }

    pub fn log_window_identity_chain(label: &str, handle: u64) {
        if handle == 0 {
            eprintln!("[PluginEditorHWNDChain] {label} hwnd=0x0 valid=false");
            return;
        }
        unsafe {
            let mut hwnd = hwnd_from(handle);
            for depth in 0..8 {
                if hwnd.0.is_null() || !IsWindow(Some(hwnd)).as_bool() {
                    eprintln!(
                        "[PluginEditorHWNDChain] {label} depth={depth} hwnd=0x{:x} valid=false",
                        hwnd.0 as u64
                    );
                    break;
                }
                let parent = GetParent(hwnd).unwrap_or_default();
                let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
                let mut pid = 0u32;
                let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
                eprintln!(
                    "[PluginEditorHWNDChain] {label} depth={depth} hwnd=0x{:x} pid={pid} tid={tid} parent=0x{:x} owner=0x{:x} class='{}' title='{}'",
                    hwnd.0 as u64,
                    parent.0 as u64,
                    owner.0 as u64,
                    class_name(hwnd),
                    window_title(hwnd)
                );
                let next = if !owner.0.is_null() { owner } else { parent };
                if next.0.is_null() || next == hwnd {
                    break;
                }
                hwnd = next;
            }
        }
    }

    pub fn focus_editor_window(handle: u64) -> bool {
        if handle == 0 {
            return false;
        }
        unsafe {
            let hwnd = hwnd_from(handle);
            if !IsWindow(Some(hwnd)).as_bool() {
                return false;
            }
            eprintln!("[NativeEditorShell] show/focus requested existing=0x{handle:x}");
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = BringWindowToTop(hwnd);
            let ok = SetForegroundWindow(hwnd).as_bool();
            let _ = SetFocus(Some(hwnd));
            eprintln!("[NativeEditorShell] foreground result={ok}");
            ok
        }
    }

    fn log_window_brief(label: &str, hwnd: HWND) {
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
            let owner = HWND(GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT) as *mut core::ffi::c_void);
            eprintln!(
                "[PluginEditor] window styles {label} hwnd=0x{:x} owner=0x{:x} style=0x{style:08x}",
                hwnd.0 as u64, owner.0 as u64
            );
        }
    }

    /// Focus the deepest plugin-owned child under the embed host HWND.
    pub fn focus_plugin_editor_child(host_hwnd: u64) {
        if host_hwnd == 0 {
            return;
        }
        unsafe {
            let host = hwnd_from(host_hwnd);
            if !IsWindow(Some(host)).as_bool() {
                return;
            }
            log_window_brief("top", host);
            let mut target = host;
            let mut child = GetWindow(host, GW_CHILD).unwrap_or_default();
            while !child.0.is_null() && IsWindow(Some(child)).as_bool() {
                target = child;
                child = GetWindow(child, GW_CHILD).unwrap_or_default();
            }
            if target != host {
                let _ = SetFocus(Some(target));
                eprintln!("[PluginEditor] focus set child=0x{:x}", target.0 as u64);
            } else {
                let _ = SetFocus(Some(host));
                eprintln!("[PluginEditor] focus set child=0x{:x}", host.0 as u64);
            }
            {
                use windows::Win32::UI::Input::KeyboardAndMouse::{GetCapture, GetFocus};
                eprintln!(
                    "[PluginEditorInput] focus=0x{:x} capture=0x{:x}",
                    GetFocus().0 as u64,
                    GetCapture().0 as u64
                );
            }
        }
    }

    /// Pump messages for the plugin editor subtree (host + descendants).
    /// Bounded: drains at most `MAX_PUMP_PER_CALL` messages per call so a
    /// message-storming plugin window can never wedge the loop here.
    pub fn pump_editor_messages(host_hwnd: u64) {
        if host_hwnd == 0 {
            return;
        }
        unsafe {
            let host = hwnd_from(host_hwnd);
            if !IsWindow(Some(host)).as_bool() {
                return;
            }
            static PUMP_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let mut pumped = 0u32;
            let mut msg = MSG::default();
            while pumped < MAX_PUMP_PER_CALL
                && PeekMessageW(&mut msg, Some(host), 0, 0, PM_REMOVE).as_bool()
            {
                // Same claim the general pump makes, and it has to be here too:
                // this drain runs first and takes the editor subtree with it, so
                // with focus on a plug-in's own child view — the normal case —
                // Space was dispatched straight into the plug-in and the
                // transport never heard about it. That is why Play/Pause from a
                // plug-in editor did nothing.
                if is_transport_toggle_key(&msg) {
                    claim_transport_toggle(Some(msg.time));
                    if plugin_debug() || key_debug() {
                        eprintln!(
                            "[Keyboard] host site=editor-pump hwnd=0x{:x} class='{}' \
                             time={} claimed=true",
                            msg.hwnd.0 as u64,
                            class_name(msg.hwnd),
                            msg.time
                        );
                    }
                    pumped += 1;
                    continue;
                }
                let _ = TranslateMessage(&msg);
                // Generic dialog routing: only treat real `#32770` dialogs as
                // dialogs; never run IsDialogMessage against plugin windows.
                if let Some(dialog) = dialog_ancestor(msg.hwnd) {
                    if IsDialogMessageW(dialog, &msg).as_bool() {
                        pumped += 1;
                        continue;
                    }
                }
                DispatchMessageW(&msg);
                pumped += 1;
            }
            if pumped > 0 {
                let n = PUMP_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if plugin_debug() && n.is_multiple_of(600) {
                    eprintln!("[PluginEditor] modal/dialog message pump active drained={pumped}");
                }
            }
        }
    }

    /// Upper bound on messages drained by any single pump call. The loop comes
    /// back within milliseconds, so capping a single drain only bounds latency
    /// for pathological message storms — it never drops messages.
    const MAX_PUMP_PER_CALL: u32 = 512;

    /// Transport-key presses claimed and not yet reported to the main app,
    /// each with the Win32 message time that identifies the physical press.
    ///
    /// Written by the editor UI thread's two message pumps and by the hook
    /// thread; drained by the IPC loop.
    ///
    /// A press, not a count: the main app watches the editors embedded in its
    /// own windows with a message hook of its own, so one Space can be seen
    /// here *and* there. The time is the same tick count in both processes, so
    /// reporting it lets the main app recognise the second report of one press
    /// instead of running play/pause twice for it -- which cancels out, and is
    /// what "Space does nothing" actually was.
    static TRANSPORT_CLAIMS: std::sync::Mutex<TransportClaims> =
        std::sync::Mutex::new(TransportClaims {
            times: [None; TRANSPORT_CLAIM_RING],
            head: 0,
            len: 0,
        });

    /// Presses held between the claim and the next IPC drain. Bounded: the drain
    /// runs every loop iteration, so anything past this is a stuck main app, and
    /// dropping the oldest is better than growing without limit inside a hook.
    const TRANSPORT_CLAIM_RING: usize = 16;

    struct TransportClaims {
        times: [Option<u32>; TRANSPORT_CLAIM_RING],
        head: usize,
        len: usize,
    }

    /// Record a Space press claimed for the DAW transport.
    ///
    /// `time_ms` is the press's Win32 message time where the claim site has one.
    /// Every claim site in this process funnels through here so there is one
    /// place that decides what a claim is.
    pub fn claim_transport_toggle(time_ms: Option<u32>) {
        let Ok(mut claims) = TRANSPORT_CLAIMS.lock() else {
            return;
        };
        let slot = (claims.head + claims.len) % TRANSPORT_CLAIM_RING;
        claims.times[slot] = time_ms;
        if claims.len == TRANSPORT_CLAIM_RING {
            claims.head = (claims.head + 1) % TRANSPORT_CLAIM_RING;
        } else {
            claims.len += 1;
        }
    }

    /// Window classes that own a text caret. Space belongs to them, never to the
    /// transport — renaming a preset must not start playback.
    fn is_text_entry_class(class: &str) -> bool {
        let lower = class.to_ascii_lowercase();
        lower == "edit"
            || lower == "combobox"
            || lower.starts_with("richedit")
            || lower.starts_with("windows.ui.core")
            || lower.contains("textbox")
    }

    /// True when `msg` is the bare Space key the DAW's transport owns.
    ///
    /// Deliberately narrow: only a fresh (non-repeating) Space with no Ctrl /
    /// Alt / Win held, and only when the focused window is not a text control.
    /// Everything else — including Space inside a plug-in's search or preset
    /// name field — is left for the plug-in.
    fn is_transport_toggle_key(msg: &MSG) -> bool {
        const VK_SPACE_W: usize = 0x20;
        const KEY_REPEAT_BIT: isize = 1 << 30;
        if (msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN) || msg.wParam.0 != VK_SPACE_W
        {
            return false;
        }
        if msg.lParam.0 & KEY_REPEAT_BIT != 0 {
            return false; // auto-repeat: one press, one toggle
        }
        unsafe {
            let held = |vk: VIRTUAL_KEY| GetKeyState(vk.0 as i32) < 0;
            if held(VK_CONTROL) || held(VK_MENU) || held(VK_LWIN) || held(VK_RWIN) {
                return false;
            }
            let focus = keyboard_focus_window();
            if !focus.is_invalid() && is_text_entry_class(&class_name(focus)) {
                return false;
            }
        }
        true
    }

    /// Transport-key presses since the last call, each with the message time
    /// identifying it where the claim site had one. The IPC loop turns each into
    /// a `HostEvent::TransportToggleRequested`.
    pub fn take_transport_toggle_requests() -> Vec<Option<u32>> {
        let mut out = Vec::new();
        if let Ok(mut claims) = TRANSPORT_CLAIMS.lock() {
            for i in 0..claims.len {
                out.push(claims.times[(claims.head + i) % TRANSPORT_CLAIM_RING]);
            }
            claims.head = 0;
            claims.len = 0;
        }
        // C++ editor shell / shared VST3 host also publishes claims here (e.g.
        // Content HWND WndProc when focus never enters the PeekMessage path).
        // That counter carries no time, so those presses go out unidentified and
        // the main app falls back to its short blind window for them.
        let from_daux =
            unsafe { daux_transport::sphere_daux_vst3_take_transport_toggle_requests() };
        out.extend(std::iter::repeat_n(None, from_daux as usize));
        out
    }

    mod daux_transport {
        unsafe extern "C" {
            pub fn sphere_daux_vst3_take_transport_toggle_requests() -> u32;
        }
    }

    #[cfg(test)]
    mod transport_key_tests {
        use super::is_text_entry_class;

        #[test]
        fn text_controls_keep_the_space_key() {
            for class in [
                "Edit",
                "EDIT",
                "ComboBox",
                "RichEdit20W",
                "RICHEDIT50W",
                "Windows.UI.Core.CoreWindow",
                "JUCE_TextBox_1",
            ] {
                assert!(
                    is_text_entry_class(class),
                    "{class} owns a caret; Space must not start playback"
                );
            }
        }

        #[test]
        fn plugin_view_classes_hand_the_space_key_to_the_transport() {
            for class in [
                "JUCE_1",
                "VSTPluginEditorWindow",
                "FutureboardDauxVst3EditorContent",
                "Static",
            ] {
                assert!(!is_text_entry_class(class), "{class} is not a text control");
            }
        }
    }

    /// Block until this thread's message queue has input or `timeout_ms`
    /// elapses. Returns `true` when woken by input. This replaces the old
    /// unconditional `sleep(8ms)` poll: the loop now idles in the kernel and
    /// wakes immediately on messages (or a `wake_ui_thread` kick), so it never
    /// spins and never adds fixed latency to plugin window input.
    pub fn wait_for_input(timeout_ms: u32) -> bool {
        unsafe {
            // MWMO_INPUTAVAILABLE: also wake for input that was already in the
            // queue when we started waiting (avoids the classic stale-QS-bits
            // missed-wakeup, which would otherwise show up as input lag).
            let result =
                MsgWaitForMultipleObjectsEx(None, timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            result == WAIT_OBJECT_0
        }
    }

    /// Wake the UI thread out of `wait_for_input` (used by the stdin reader
    /// thread when a new IPC command arrives). WM_NULL is a no-op message.
    pub fn wake_ui_thread(thread_id: u64) {
        unsafe {
            let _ = PostThreadMessageW(thread_id as u32, WM_NULL, WPARAM(0), LPARAM(0));
        }
    }

    /// Capture safety on editor open (spec: capture should be null or
    /// plugin-owned while interacting). Logs the current focus/capture once;
    /// if an HWND *unrelated* to the new editor holds mouse capture on this
    /// thread, release it. Never sets capture.
    pub fn log_capture_on_open(host_hwnd: u64) {
        unsafe {
            let capture = GetCapture();
            let focus = GetFocus();
            eprintln!(
                "[PluginEditorInput] editor_open focus=0x{:x} capture=0x{:x}",
                focus.0 as u64, capture.0 as u64
            );
            if capture.0.is_null() {
                return;
            }
            let host = hwnd_from(host_hwnd);
            let related = host_hwnd != 0 && (capture == host || IsChild(host, capture).as_bool());
            if !related {
                let _ = ReleaseCapture();
                eprintln!(
                    "[PluginEditorInput] released_unrelated_capture=0x{:x}",
                    capture.0 as u64
                );
            }
        }
    }

    /// Debug classification of a message target: wrapper chrome, our embed
    /// host windows, a dialog, or a plugin-owned window.
    fn classify_target(hwnd: HWND) -> &'static str {
        let class = class_name(hwnd);
        match class.as_str() {
            "FutureboardDauxVst3EditorContent" | "FutureboardDauxVst3EditorChild" => "embed_host",
            "FutureboardDauxVst3EditorDetached" => "embed_top",
            "SpherePluginEditorShell" | "SpherePluginEditorContent" => "wrapper",
            "#32770" => "dialog",
            _ => "plugin_owned",
        }
    }

    fn log_input_dispatch(msg: &MSG) {
        // Default dispatch logging covers only mouse button / key messages —
        // never per-move/per-timer/per-paint floods (those are throttled
        // separately in `log_throttled_noise`).
        let interesting = matches!(
            msg.message,
            WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_RBUTTONUP
                | WM_MBUTTONDOWN
                | WM_KEYDOWN
        );
        if !interesting {
            return;
        }
        let x = (msg.lParam.0 & 0xFFFF) as i16 as i32;
        let y = ((msg.lParam.0 >> 16) & 0xFFFF) as i16 as i32;
        let root = unsafe { GetAncestor(msg.hwnd, GA_ROOT) };
        eprintln!(
            "[PluginUIThread] dispatch hwnd=0x{:x} msg=0x{:04x} target={} class='{}' \
             client=({x},{y}) screen=({},{}) root=0x{:x}",
            msg.hwnd.0 as u64,
            msg.message,
            classify_target(msg.hwnd),
            class_name(msg.hwnd),
            msg.pt.x,
            msg.pt.y,
            root.0 as u64,
        );
    }

    /// Throttled high-frequency message tracing (debug mode, not safe mode):
    /// WM_MOUSEMOVE and WM_TIMER at most 2/sec each.
    fn log_throttled_noise(msg: &MSG) {
        static MOUSE_MOVE_RATE: LogRate = LogRate::new(2);
        static TIMER_RATE: LogRate = LogRate::new(2);
        let rate = match msg.message {
            WM_MOUSEMOVE => &MOUSE_MOVE_RATE,
            WM_TIMER => &TIMER_RATE,
            _ => return,
        };
        if rate.allow() {
            eprintln!(
                "[PluginUIThread] trace hwnd=0x{:x} msg=0x{:04x} class='{}' (throttled 2/sec)",
                msg.hwnd.0 as u64,
                msg.message,
                class_name(msg.hwnd),
            );
        }
    }

    /// Click-path diagnostic (spec item 9): for a left click, log everything
    /// needed to tell wrong-hit-test / disabled-window / focus-capture /
    /// wrong-thread / consumed-by-dialog-routing apart. Throttled to 4/sec.
    fn log_click_path(msg: &MSG) {
        unsafe {
            let pt = msg.pt; // screen coordinates of the click
            let wfp = WindowFromPoint(pt);
            let focus = GetFocus();
            let capture = GetCapture();
            let mut target_pid = 0u32;
            let target_tid = GetWindowThreadProcessId(msg.hwnd, Some(&mut target_pid));
            let our_tid = windows::Win32::System::Threading::GetCurrentThreadId();
            eprintln!(
                "[PluginClickPath] screen=({},{}) msg_hwnd=0x{:x} class='{}' target={} \
                 enabled={} visible={} target_tid={target_tid} target_pid={target_pid} \
                 our_tid={our_tid} same_thread={}",
                pt.x,
                pt.y,
                msg.hwnd.0 as u64,
                class_name(msg.hwnd),
                classify_target(msg.hwnd),
                IsWindowEnabled(msg.hwnd).as_bool(),
                IsWindowVisible(msg.hwnd).as_bool(),
                target_tid == our_tid,
            );
            eprintln!(
                "[PluginClickPath] window_from_point=0x{:x} wfp_class='{}' wfp_enabled={} \
                 focus=0x{:x} capture=0x{:x}",
                wfp.0 as u64,
                class_name(wfp),
                IsWindowEnabled(wfp).as_bool(),
                focus.0 as u64,
                capture.0 as u64,
            );
            // Hit-test the wrapper (cross-process top-level) and each editor
            // root so a wrong/covered hit target is visible in one log line.
            let wrapper = GetAncestor(msg.hwnd, GA_ROOT);
            let mut probes: Vec<(&'static str, HWND)> = vec![("wrapper", wrapper)];
            let roots = editor_roots();
            for root in &roots {
                probes.push(("editor_root", hwnd_from(*root)));
            }
            for (label, probe) in probes {
                if probe.0.is_null() || !IsWindow(Some(probe)).as_bool() {
                    continue;
                }
                let mut client = pt;
                let _ = windows::Win32::Graphics::Gdi::ScreenToClient(probe, &mut client);
                let child = ChildWindowFromPointEx(probe, client, CWP_ALL);
                eprintln!(
                    "[PluginClickPath] {label}=0x{:x} child_from_point=0x{:x} child_class='{}'",
                    probe.0 as u64,
                    child.0 as u64,
                    class_name(child),
                );
            }
        }
    }

    /// Non-blocking drain of this thread's message queue. Returns the number
    /// of messages dispatched (pump-gap watchdog input). Bounded per call.
    pub fn pump_messages() -> u32 {
        let debug = plugin_debug();
        let safe = editor_safe_mode();
        static CLICK_PATH_RATE: LogRate = LogRate::new(4);
        let mut dispatched = 0u32;
        unsafe {
            let mut msg = MSG::default();
            while dispatched < MAX_PUMP_PER_CALL
                && PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool()
            {
                if debug && !safe {
                    log_throttled_noise(&msg);
                }
                if debug {
                    log_input_dispatch(&msg);
                }
                // Transport key: claim it before the plug-in sees it, so Space
                // means play/pause with an editor focused exactly as it does in
                // the arrangement. Swallowed here and reported over IPC.
                if is_transport_toggle_key(&msg) {
                    claim_transport_toggle(Some(msg.time));
                    if debug || key_debug() {
                        eprintln!(
                            "[Keyboard] host site=thread-pump hwnd=0x{:x} class='{}' \
                             time={} claimed=true",
                            msg.hwnd.0 as u64,
                            class_name(msg.hwnd),
                            msg.time
                        );
                    }
                    dispatched += 1;
                    continue;
                }
                // Focus/capture + hit-test summary on click. Explicit debug
                // only, and throttled so a click storm cannot flood stderr.
                let click_diag = debug && msg.message == WM_LBUTTONDOWN && CLICK_PATH_RATE.allow();
                if click_diag {
                    log_click_path(&msg);
                }
                let _ = TranslateMessage(&msg);
                // `IsDialogMessageW(msg.hwnd, …)` treated EVERY window as a
                // dialog, swallowing Tab/arrow/Enter/Escape keystrokes that
                // belong to plugin editor controls. Only route through real
                // `#32770` dialogs in the target's parent chain, and never
                // consume a message that IsDialogMessage did not handle.
                let mut dialog_candidate = 0u64;
                let mut dialog_handled = false;
                if let Some(dialog) = dialog_ancestor(msg.hwnd) {
                    dialog_candidate = dialog.0 as u64;
                    static DIALOG_RATE: LogRate = LogRate::new(1);
                    if debug && DIALOG_RATE.allow() {
                        eprintln!("[PluginUIThread] dialog candidate hwnd=0x{dialog_candidate:x}");
                    }
                    dialog_handled = IsDialogMessageW(dialog, &msg).as_bool();
                    if dialog_handled && debug && !safe {
                        eprintln!(
                            "[PluginUIThread] IsDialogMessage handled msg=0x{:04x} hwnd=0x{:x}",
                            msg.message, msg.hwnd.0 as u64
                        );
                    }
                }
                if click_diag {
                    eprintln!(
                        "[PluginClickPath] dialog_candidate=0x{dialog_candidate:x} \
                         is_dialog_message_handled={dialog_handled} dispatched={}",
                        !dialog_handled
                    );
                }
                if dialog_handled {
                    dispatched += 1;
                    continue;
                }
                DispatchMessageW(&msg);
                dispatched += 1;
            }
        }
        dispatched
    }

    /// One-shot window/thread state snapshot (spec item 8): wrapper, embed
    /// child, dialogs, and descendants with class/style/parent/owner/enabled/
    /// visible/rect/thread/process. Triggered once per editor open and from
    /// the pump-gap watchdog — throttled, never per-frame.
    pub fn plugin_editor_snapshot(reason: &str) {
        if !plugin_debug() {
            return;
        }
        static SNAPSHOT_RATE: LogRate = LogRate::new(1);
        if !SNAPSHOT_RATE.allow() {
            return;
        }
        const MAX_WINDOWS: usize = 64;
        fn snapshot_one(label: &str, hwnd: HWND, count: &mut usize) {
            if hwnd.0.is_null() || *count >= MAX_WINDOWS {
                return;
            }
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    return;
                }
                *count += 1;
                let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                let parent = GetParent(hwnd).unwrap_or_default();
                let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
                let mut rect = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rect);
                let mut pid = 0u32;
                let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
                eprintln!(
                    "[PluginEditorSnapshot] {label} hwnd=0x{:x} class='{}' style=0x{style:08x} \
                     exstyle=0x{exstyle:08x} parent=0x{:x} owner=0x{:x} enabled={} visible={} \
                     rect=({},{},{},{}) tid={tid} pid={pid}",
                    hwnd.0 as u64,
                    class_name(hwnd),
                    parent.0 as u64,
                    owner.0 as u64,
                    IsWindowEnabled(hwnd).as_bool(),
                    IsWindowVisible(hwnd).as_bool(),
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.bottom,
                );
            }
        }
        struct SnapCtx {
            count: usize,
        }
        unsafe extern "system" fn snap_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let ctx = unsafe { &mut *(lparam.0 as *mut SnapCtx) };
            if ctx.count >= MAX_WINDOWS {
                return BOOL(0);
            }
            snapshot_one("descendant", hwnd, &mut ctx.count);
            BOOL(1)
        }
        unsafe extern "system" fn snap_thread_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let ctx = unsafe { &mut *(lparam.0 as *mut SnapCtx) };
            if ctx.count >= MAX_WINDOWS {
                return BOOL(0);
            }
            snapshot_one("thread_window", hwnd, &mut ctx.count);
            unsafe {
                let _ = EnumChildWindows(Some(hwnd), Some(snap_child), lparam);
            }
            BOOL(1)
        }
        let roots = editor_roots();
        eprintln!(
            "[PluginEditorSnapshot] begin reason={reason} editor_roots={}",
            roots.len()
        );
        let mut ctx = SnapCtx { count: 0 };
        unsafe {
            for root in &roots {
                let root_hwnd = hwnd_from(*root);
                if !IsWindow(Some(root_hwnd)).as_bool() {
                    continue;
                }
                // GA_ROOT crosses the process boundary to the main-app wrapper.
                let wrapper = GetAncestor(root_hwnd, GA_ROOT);
                snapshot_one("wrapper", wrapper, &mut ctx.count);
                if wrapper != root_hwnd {
                    snapshot_one("embed_root", root_hwnd, &mut ctx.count);
                }
                let _ = EnumChildWindows(
                    Some(wrapper),
                    Some(snap_child),
                    LPARAM(&mut ctx as *mut SnapCtx as isize),
                );
            }
            // Popups/dialogs the plugin created on this UI thread (not under
            // the wrapper tree — e.g. #32770 file dialogs, license prompts).
            let tid = windows::Win32::System::Threading::GetCurrentThreadId();
            let _ = EnumThreadWindows(
                tid,
                Some(snap_thread_window),
                LPARAM(&mut ctx as *mut SnapCtx as isize),
            );
            let focus = GetFocus();
            let capture = GetCapture();
            eprintln!(
                "[PluginEditorSnapshot] end windows={} focus=0x{:x} capture=0x{:x}",
                ctx.count, focus.0 as u64, capture.0 as u64
            );
        }
    }

    /// Debug helper: diff the set of windows on this UI thread plus every
    /// descendant of the given editor roots against `known`, logging windows
    /// that appeared or disappeared. Confirms plugin-created popups/dialogs
    /// exist, are enabled, and live in the expected tree — no vendor logic.
    pub fn log_window_tree_changes(
        roots: &[u64],
        known: &mut std::collections::HashMap<u64, String>,
    ) {
        unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let set = unsafe { &mut *(lparam.0 as *mut Vec<u64>) };
            set.push(hwnd.0 as u64);
            unsafe {
                let _ = EnumChildWindows(Some(hwnd), Some(collect_children), lparam);
            }
            BOOL(1)
        }
        unsafe extern "system" fn collect_children(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let set = unsafe { &mut *(lparam.0 as *mut Vec<u64>) };
            set.push(hwnd.0 as u64);
            BOOL(1)
        }
        let mut current: Vec<u64> = Vec::with_capacity(64);
        unsafe {
            let tid = windows::Win32::System::Threading::GetCurrentThreadId();
            let _ = EnumThreadWindows(
                tid,
                Some(collect),
                LPARAM(&mut current as *mut Vec<u64> as isize),
            );
            for root in roots {
                if *root != 0 && IsWindow(Some(hwnd_from(*root))).as_bool() {
                    current.push(*root);
                    let _ = EnumChildWindows(
                        Some(hwnd_from(*root)),
                        Some(collect_children),
                        LPARAM(&mut current as *mut Vec<u64> as isize),
                    );
                }
            }
        }
        let current: std::collections::HashSet<u64> = current.into_iter().collect();
        for hwnd_v in &current {
            if known.contains_key(hwnd_v) {
                continue;
            }
            let hwnd = hwnd_from(*hwnd_v);
            let class = class_name(hwnd);
            unsafe {
                let parent = GetParent(hwnd).unwrap_or_default();
                let owner = GetWindow(hwnd, GW_OWNER).unwrap_or_default();
                eprintln!(
                    "[PluginEditorWindowTree] child hwnd=0x{hwnd_v:x} class='{class}' \
                     parent=0x{:x} owner=0x{:x} enabled={} visible={}",
                    parent.0 as u64,
                    owner.0 as u64,
                    IsWindowEnabled(hwnd).as_bool(),
                    IsWindowVisible(hwnd).as_bool(),
                );
            }
            known.insert(*hwnd_v, class);
        }
        known.retain(|hwnd_v, class| {
            if current.contains(hwnd_v) {
                return true;
            }
            eprintln!("[PluginEditorWindowTree] gone hwnd=0x{hwnd_v:x} class='{class}'");
            false
        });
    }

    /// Create a top window + a real WS_CHILD content window using the
    /// predefined `STATIC` class (no RegisterClass/WndProc needed). Returns
    /// `(top_hwnd, content_hwnd)` as `u64`s.
    pub fn create_selftest_windows() -> Option<(u64, u64)> {
        unsafe {
            let top = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Futureboard Plugin Host Selftest"),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                820,
                640,
                None,
                None,
                None,
                None,
            )
            .ok()?;

            let content = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                0,
                0,
                800,
                600,
                Some(top),
                None,
                None,
                None,
            )
            .ok()?;

            Some((top.0 as u64, content.0 as u64))
        }
    }

    pub fn destroy_selftest_windows(top: u64, content: u64) {
        unsafe {
            if content != 0 {
                let _ = DestroyWindow(hwnd_from(content));
            }
            if top != 0 {
                let _ = DestroyWindow(hwnd_from(top));
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    /// AppKit application + cooperative event pump for this process. Plug-in
    /// editor windows on macOS are AppKit windows owned here, so the IPC loop
    /// must drain the NSApplication event queue or the editor never appears.
    #[cfg(target_os = "macos")]
    mod appkit {
        unsafe extern "C" {
            pub fn sphere_plugin_host_mac_ui_init();
            pub fn sphere_plugin_host_mac_ui_pump() -> std::os::raw::c_uint;
            pub fn sphere_plugin_host_mac_ui_wait(
                timeout_ms: std::os::raw::c_uint,
            ) -> std::os::raw::c_int;
            pub fn sphere_plugin_host_mac_ui_wake();
            pub fn sphere_plugin_host_mac_ui_focus_window(handle: u64) -> std::os::raw::c_int;
            pub fn sphere_plugin_host_mac_ui_take_transport_toggles() -> std::os::raw::c_uint;
        }
    }

    /// Transport-key presses claimed from plug-in editor windows since the last
    /// call. Platform pumps + the process-wide C++ VST3 counter both feed this.
    ///
    /// Neither of these carries a message time, so every press goes out
    /// unidentified and the main app deduplicates them by its short blind
    /// window. Only the Windows path can do better, and only because Win32 hands
    /// it a per-press tick count that means the same thing in both processes.
    pub fn take_transport_toggle_requests() -> Vec<Option<u32>> {
        let from_platform = {
            #[cfg(target_os = "macos")]
            {
                unsafe { appkit::sphere_plugin_host_mac_ui_take_transport_toggles() as u32 }
            }
            #[cfg(not(target_os = "macos"))]
            {
                0u32
            }
        };
        let from_daux =
            unsafe { daux_transport::sphere_daux_vst3_take_transport_toggle_requests() };
        let total = from_platform.saturating_add(from_daux) as usize;
        vec![None; total]
    }

    mod daux_transport {
        unsafe extern "C" {
            pub fn sphere_daux_vst3_take_transport_toggle_requests() -> u32;
        }
    }

    /// Windows initializes COM here; macOS brings up AppKit. Both give the UI
    /// thread what the platform's plug-in editors require before any view exists.
    pub fn com_init() {
        #[cfg(target_os = "macos")]
        unsafe {
            appkit::sphere_plugin_host_mac_ui_init()
        };
    }
    pub fn ensure_dpi_awareness() {}
    pub fn system_dpi() -> u32 {
        96
    }
    pub fn com_uninit() {}
    pub fn current_thread_id() -> u64 {
        0
    }
    pub fn is_process_alive(_pid: u32) -> bool {
        true
    }
    pub fn is_window(handle: u64) -> bool {
        handle != 0
    }
    pub fn window_process_id(_handle: u64) -> Option<u32> {
        None
    }
    pub fn log_window_identity_chain(_label: &str, _handle: u64) {}
    pub fn focus_editor_window(handle: u64) -> bool {
        if handle == 0 {
            return false;
        }
        #[cfg(target_os = "macos")]
        {
            // AU editors store an NSWindow* as the handle. VST3 stores an opaque
            // counter — that path returns false here and the caller falls back to
            // `processor.focus_editor()`.
            let ok = unsafe { appkit::sphere_plugin_host_mac_ui_focus_window(handle) != 0 };
            eprintln!("[NativeEditorShell] mac focus handle=0x{handle:x} result={ok}");
            return ok;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = handle;
            false
        }
    }
    pub fn pump_messages() -> u32 {
        #[cfg(target_os = "macos")]
        {
            unsafe { appkit::sphere_plugin_host_mac_ui_pump() as u32 }
        }
        #[cfg(not(target_os = "macos"))]
        {
            0
        }
    }
    pub fn plugin_debug() -> bool {
        false
    }
    pub fn editor_safe_mode() -> bool {
        false
    }

    /// Coarse log rate limiter: allows at most `max` events per second.
    pub struct LogRate {
        window_start_ms: std::sync::atomic::AtomicU64,
        count: std::sync::atomic::AtomicU32,
        max_per_sec: u32,
    }

    impl LogRate {
        pub const fn new(max_per_sec: u32) -> Self {
            Self {
                window_start_ms: std::sync::atomic::AtomicU64::new(0),
                count: std::sync::atomic::AtomicU32::new(0),
                max_per_sec,
            }
        }

        pub fn allow(&self) -> bool {
            use std::sync::atomic::Ordering;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let start = self.window_start_ms.load(Ordering::Relaxed);
            if now.saturating_sub(start) >= 1000 {
                self.window_start_ms.store(now, Ordering::Relaxed);
                self.count.store(1, Ordering::Relaxed);
                return true;
            }
            self.count.fetch_add(1, Ordering::Relaxed) < self.max_per_sec
        }
    }

    pub fn set_editor_roots(_roots: Vec<u64>) {}
    pub fn plugin_editor_snapshot(_reason: &str) {}
    pub fn log_capture_on_open(_host_hwnd: u64) {}
    pub fn wake_ui_thread(_thread_id: u64) {
        #[cfg(target_os = "macos")]
        unsafe {
            appkit::sphere_plugin_host_mac_ui_wake()
        };
    }
    /// Wait for UI input, bounded by `timeout_ms`. On macOS this also runs the
    /// main run loop, which is what services plug-in timers, Core Animation
    /// commits and main-queue blocks while an editor is open.
    pub fn wait_for_input(timeout_ms: u32) -> bool {
        #[cfg(target_os = "macos")]
        {
            unsafe { appkit::sphere_plugin_host_mac_ui_wait(timeout_ms) != 0 }
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
            false
        }
    }
    pub fn log_window_tree_changes(
        _roots: &[u64],
        _known: &mut std::collections::HashMap<u64, String>,
    ) {
    }
    pub fn focus_plugin_editor_child(_host_hwnd: u64) {}
    pub fn pump_editor_messages(_host_hwnd: u64) {
        // Drain AppKit during attach/load so plugins that need nested run-loop
        // work (timers, view layout) can progress while the IPC thread is busy.
        #[cfg(target_os = "macos")]
        unsafe {
            let _ = appkit::sphere_plugin_host_mac_ui_pump();
        }
    }
    pub fn create_selftest_windows() -> Option<(u64, u64)> {
        None
    }
    pub fn destroy_selftest_windows(_top: u64, _content: u64) {}
}
