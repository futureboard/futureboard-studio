//! Manual probe: run a plug-in through the plug-in host exactly as Studio does,
//! without Studio.
//!
//! For each path given it:
//!
//! 1. scans it with `FutureboardPluginScanner` for its class id;
//! 2. spawns `FutureboardPluginHostX64` through the app's own
//!    [`PluginHostClient`] — the same spawn, environment and event reader
//!    Studio uses;
//! 3. loads the plug-in and prepares it at 48 kHz / 512;
//! 4. unless `--no-editor`, opens its editor into a window this probe owns
//!    (the role Studio's editor shell plays) and keeps it up for a few
//!    seconds while pumping messages;
//! 5. closes the editor, unloads, shuts the host down, and reports how the
//!    host exited, with the tail of its log when it did not exit cleanly.
//!
//! ```text
//! cargo build -p sphere-plugin-host --bins
//! cargo run -p sphere-plugin-host --example plugin_host_probe -- \
//!     "C:\Program Files\Common Files\VST3\Mix DRUMS.vst3" [--no-editor] [--seconds 4]
//! ```
//!
//! A path may be a directory: every `.vst3` directly inside it is probed.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use SpherePluginHost::ipc::HostEvent;
use SpherePluginHost::plugin_host_client::{ClientEvent, PluginHostClient};

const INSTANCE: &str = "probe-1";
const SAMPLE_RATE: u32 = 48_000;
const BLOCK: u32 = 512;

#[derive(Debug, Default)]
struct Outcome {
    loaded: Option<Result<String, String>>,
    prepared: Option<bool>,
    editor: Option<Result<String, String>>,
    host_lost: Option<&'static str>,
    exit: Option<String>,
}

fn main() {
    let mut paths = Vec::new();
    let mut editor = true;
    let mut seconds = 4.0f32;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-editor" => editor = false,
            "--seconds" => seconds = args.next().and_then(|s| s.parse().ok()).unwrap_or(seconds),
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    if paths.is_empty() {
        eprintln!("usage: plugin_host_probe <plugin.vst3 | folder>... [--no-editor] [--seconds N]");
        std::process::exit(2);
    }
    let plugins: Vec<PathBuf> = paths
        .into_iter()
        .flat_map(|path| {
            if path.is_dir() && path.extension().is_none_or(|ext| ext != "vst3") {
                let mut inside: Vec<PathBuf> = std::fs::read_dir(&path)
                    .map(|dir| {
                        dir.filter_map(Result::ok)
                            .map(|entry| entry.path())
                            .filter(|p| p.extension().is_some_and(|ext| ext == "vst3"))
                            .collect()
                    })
                    .unwrap_or_default();
                inside.sort();
                inside
            } else {
                vec![path]
            }
        })
        .collect();

    let host = SpherePluginHost::plugin_host_client::locate_plugin_host_binary()
        .unwrap_or_else(|error| {
            eprintln!("{error} — build it with `cargo build -p sphere-plugin-host --bins`");
            std::process::exit(2);
        });
    let scanner = host.with_file_name(if cfg!(windows) {
        "FutureboardPluginScanner.exe"
    } else {
        "FutureboardPluginScanner"
    });

    let mut failures = 0;
    for plugin in &plugins {
        println!("\n=== {}", plugin.display());
        let outcome = probe(&host, &scanner, plugin, editor, seconds);
        let ok = matches!(outcome.loaded, Some(Ok(_)))
            && outcome.prepared == Some(true)
            && (!editor || matches!(outcome.editor, Some(Ok(_))))
            && outcome.host_lost.is_none();
        if !ok {
            failures += 1;
        }
        println!("  load     {:?}", outcome.loaded);
        println!("  prepare  {:?}", outcome.prepared);
        if editor {
            println!("  editor   {:?}", outcome.editor);
        }
        println!("  host     lost={:?} exit={:?}", outcome.host_lost, outcome.exit);
        println!("  RESULT   {}", if ok { "OK" } else { "FAILED" });
    }
    println!("\n{} probed, {failures} failed", plugins.len());
    std::process::exit(i32::from(failures > 0));
}

/// The class id of the plug-in's first audio class, from the isolated scanner.
fn scan_class_id(scanner: &Path, plugin: &Path) -> Result<(String, String), String> {
    let output = std::process::Command::new(scanner)
        .args(["--format", "vst3", "--json", "--path"])
        .arg(plugin)
        .output()
        .map_err(|error| format!("scanner did not run: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let payload = stdout
        .lines()
        .find_map(|line| line.strip_prefix("@@FUTUREBOARD_SCAN_PAYLOAD@@"))
        .ok_or_else(|| format!("scanner gave no payload (exit {:?})", output.status.code()))?;
    let json: serde_json::Value =
        serde_json::from_str(payload).map_err(|error| format!("bad payload: {error}"))?;
    let entry = json["plugins"]
        .as_array()
        .and_then(|plugins| plugins.first())
        .ok_or_else(|| format!("scanner found no class: {}", json["failures"]))?;
    Ok((
        entry["classId"].as_str().unwrap_or_default().to_string(),
        entry["name"].as_str().unwrap_or_default().to_string(),
    ))
}

/// Waits for an event `pick` accepts, pumping window messages meanwhile.
fn wait_for<T>(
    client: &PluginHostClient,
    outcome: &mut Outcome,
    timeout: Duration,
    mut pick: impl FnMut(&HostEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(event) = client.try_recv_event() {
            match event {
                ClientEvent::Host(event) => {
                    if let HostEvent::Log { level, message } = &event {
                        println!("  host log [{level}] {message}");
                    }
                    if let Some(found) = pick(&event) {
                        return Some(found);
                    }
                }
                ClientEvent::Disconnected => {
                    outcome.host_lost.get_or_insert("host disconnected");
                    return None;
                }
            }
        }
        window::pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn probe(host: &Path, scanner: &Path, plugin: &Path, editor: bool, seconds: f32) -> Outcome {
    let mut outcome = Outcome::default();
    let (class_id, name) = match scan_class_id(scanner, plugin) {
        Ok(found) => found,
        Err(error) => {
            outcome.loaded = Some(Err(format!("scan: {error}")));
            return outcome;
        }
    };
    println!("  scanned  name={name} class={class_id}");
    let path = plugin.to_string_lossy().to_string();
    let mut client = match PluginHostClient::spawn_from(host) {
        Ok(client) => client,
        Err(error) => {
            outcome.loaded = Some(Err(format!("spawn: {error}")));
            return outcome;
        }
    };
    let pid = client.pid();
    println!("  host pid {pid}");

    let _ = client.load_plugin(INSTANCE, &path, &class_id, SAMPLE_RATE, BLOCK, Some("VST3".to_string()));
    outcome.loaded = wait_for(&client, &mut outcome, Duration::from_secs(60), |event| match event {
        HostEvent::PluginLoaded { name, .. } | HostEvent::PluginAlreadyLoaded { name, .. } => {
            Some(Ok(name.clone()))
        }
        HostEvent::PluginLoadFailed { error, .. } => Some(Err(error.clone())),
        _ => None,
    })
    .or_else(|| Some(Err("no answer".to_string())));

    if matches!(outcome.loaded, Some(Ok(_))) {
        let _ = client.prepare_processing(INSTANCE, SAMPLE_RATE, BLOCK, 2, 2);
        outcome.prepared = wait_for(&client, &mut outcome, Duration::from_secs(30), |event| {
            matches!(event, HostEvent::ProcessingPrepared { .. }).then_some(true)
        })
        .or(Some(false));
    }

    if editor && outcome.host_lost.is_none() && matches!(outcome.loaded, Some(Ok(_))) {
        match window::open(&name) {
            Some((top, content)) => {
                let _ = client.open_editor_with_metadata(
                    "probe-track",
                    Some(0),
                    Some("Probe".to_string()),
                    "probe-slot",
                    INSTANCE,
                    &path,
                    &class_id,
                    None,
                    name.clone(),
                    // As Studio sends it: the editor's content window is both
                    // the owner reference and the parent.
                    content,
                    content,
                    800,
                    600,
                    96,
                );
                outcome.editor =
                    wait_for(&client, &mut outcome, Duration::from_secs(30), |event| match event {
                        HostEvent::EditorAttached { result, .. } => {
                            Some(Ok(format!("attached result={result}")))
                        }
                        HostEvent::EditorAttachFailed { error, .. } => Some(Err(error.clone())),
                        _ => None,
                    })
                    .or_else(|| Some(Err("no answer".to_string())));
                if matches!(outcome.editor, Some(Ok(_))) {
                    // Let it settle, then say what the plug-in built in the
                    // window: an editor that attached but drew nothing shows
                    // here as a hidden, empty or off-place child.
                    let _ = wait_for(&client, &mut outcome, Duration::from_millis(800), |_| {
                        None::<()>
                    });
                    for line in window::describe(content, pid) {
                        println!("  window   {line}");
                    }
                    // Keep it up, the way a user looks at it.
                    let _ = wait_for(
                        &client,
                        &mut outcome,
                        Duration::from_secs_f32(seconds),
                        |event| match event {
                            HostEvent::EditorUnresponsive { .. } => Some(()),
                            _ => None,
                        },
                    );
                    let _ = client.close_editor(INSTANCE);
                    let _ = wait_for(&client, &mut outcome, Duration::from_secs(10), |event| {
                        matches!(event, HostEvent::EditorClosed { .. }).then_some(())
                    });
                }
                window::close(top, content);
            }
            None => outcome.editor = Some(Err("probe window could not be made".to_string())),
        }
    }

    if outcome.host_lost.is_none() {
        let _ = client.unload_plugin(INSTANCE);
        let _ = wait_for(&client, &mut outcome, Duration::from_secs(10), |event| {
            matches!(event, HostEvent::PluginUnloaded { .. }).then_some(())
        });
    }
    let _ = client.shutdown();
    outcome.exit = Some(match client.wait_for_exit() {
        Ok(status) => match status.code() {
            Some(code) => format!("code {code} (0x{:08X})", code as u32),
            None => "killed".to_string(),
        },
        Err(error) => format!("unknown: {error}"),
    });
    let clean = outcome.exit.as_deref() == Some("code 0 (0x00000000)");
    if !clean || outcome.host_lost.is_some() {
        print_log_tail(host, pid);
    }
    outcome
}

/// The host's own log, beside its binary (`plugin_host_logging`).
fn print_log_tail(host: &Path, pid: u32) {
    let log = host
        .parent()
        .unwrap_or(Path::new("."))
        .join("logs")
        .join("plugin-host")
        .join(format!("{pid}.log"));
    match std::fs::read_to_string(&log) {
        Ok(text) => {
            println!("  --- host log tail ({}) ---", log.display());
            let lines: Vec<&str> = text.lines().collect();
            for line in &lines[lines.len().saturating_sub(30)..] {
                println!("  | {line}");
            }
        }
        Err(error) => println!("  (no host log at {}: {error})", log.display()),
    }
}

/// The window the editor is parented into: Studio's editor shell, reduced
/// to a top-level window and a content child.
#[cfg(windows)]
mod window {
    use windows::core::{PCWSTR, w};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        CW_USEDEFAULT, CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
        PeekMessageW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
        WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    pub fn open(title: &str) -> Option<(u64, u64)> {
        let title: Vec<u16> = format!("Plugin host probe — {title}")
            .encode_utf16()
            .chain([0])
            .collect();
        unsafe {
            let top = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR(title.as_ptr()),
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
            let _ = ShowWindow(top, SW_SHOWNOACTIVATE);
            Some((top.0 as u64, content.0 as u64))
        }
    }

    /// The probe's content window, then every window the host process owns
    /// — the editor shell it opens and what the plug-in built inside it —
    /// with class, screen rect, visibility and style.
    pub fn describe(content: u64, host_pid: u32) -> Vec<String> {
        use windows::Win32::Foundation::{LPARAM, RECT};
        use windows::core::BOOL;
        use windows::Win32::UI::WindowsAndMessaging::{
            EnumChildWindows, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetParent, GetWindowLongPtrW,
            GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
        };
        unsafe extern "system" fn visit(hwnd: HWND, lines: LPARAM) -> BOOL {
            unsafe {
                let lines = &mut *(lines.0 as *mut Vec<(HWND, String)>);
                let mut class = [0u16; 128];
                let len = GetClassNameW(hwnd, &mut class) as usize;
                let mut rect = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rect);
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                lines.push((
                    hwnd,
                    format!(
                        "hwnd=0x{:x} parent=0x{:x} pid={pid} class={} screen=({},{} {}x{}) visible={} style=0x{:08x} ex=0x{:08x}",
                        hwnd.0 as usize,
                        GetParent(hwnd).map(|h| h.0 as usize).unwrap_or(0),
                        String::from_utf16_lossy(&class[..len]),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        IsWindowVisible(hwnd).as_bool(),
                        GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
                        GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32,
                    ),
                ));
            }
            BOOL(1)
        }
        let mut found: Vec<(HWND, String)> = Vec::new();
        unsafe {
            let parent = HWND(content as *mut _);
            let mut rect = RECT::default();
            let _ = GetWindowRect(parent, &mut rect);
            found.push((
                parent,
                format!(
                    "content screen=({},{} {}x{}) visible={}",
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    IsWindowVisible(parent).as_bool()
                ),
            ));
            let _ = EnumChildWindows(
                Some(parent),
                Some(visit),
                LPARAM(&mut found as *mut _ as isize),
            );
            // The host's own top-level windows and everything under them.
            let mut tops: Vec<(HWND, String)> = Vec::new();
            let _ = windows::Win32::UI::WindowsAndMessaging::EnumWindows(
                Some(visit),
                LPARAM(&mut tops as *mut _ as isize),
            );
            for (top, line) in tops {
                let mut pid = 0u32;
                GetWindowThreadProcessId(top, Some(&mut pid));
                if pid != host_pid {
                    continue;
                }
                found.push((top, format!("host top {line}")));
                let mut children: Vec<(HWND, String)> = Vec::new();
                let _ = EnumChildWindows(
                    Some(top),
                    Some(visit),
                    LPARAM(&mut children as *mut _ as isize),
                );
                found.extend(children.into_iter().map(|(h, l)| (h, format!("  {l}"))));
            }
        }
        found.into_iter().map(|(_, line)| line).collect()
    }

    pub fn pump() {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub fn close(top: u64, content: u64) {
        unsafe {
            let _ = DestroyWindow(HWND(content as *mut _));
            let _ = DestroyWindow(HWND(top as *mut _));
        }
    }
}

#[cfg(not(windows))]
mod window {
    pub fn open(_title: &str) -> Option<(u64, u64)> {
        None
    }
    pub fn describe(_content: u64, _host_pid: u32) -> Vec<String> {
        Vec::new()
    }
    pub fn pump() {}
    pub fn close(_top: u64, _content: u64) {}
}
