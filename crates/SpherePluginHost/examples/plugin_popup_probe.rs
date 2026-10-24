//! Manual probe: does a plug-in's popup menu stay open when its editor is
//! embedded the way Studio embeds it — a child of a window in another process?
//!
//! Spawns `FutureboardPluginHostX64` through the app's own [`PluginHostClient`]
//! with `FUTUREBOARD_FOCUS_TRACE=1`, loads the plug-in and opens its editor
//! into a window this probe owns, placed at a fixed spot on screen. Then:
//!
//! - `--snapshot <file.bmp>` saves what that window shows, to find a control;
//! - `--click X,Y` clicks there (editor client pixels) with real input and
//!   records, every 2 ms for `--watch` ms, which top-level windows of the host
//!   process are visible — a plug-in's popup is one — and prints the host's
//!   focus trace (`%TEMP%\futureboard-focus-trace.log`) from the click on.
//!
//! The parent can act like Studio's editor window: `--studio` gives the
//! content window Studio's focus forwarding (`plugin_content_host`) and the
//! top-level window GPUI's `WM_MOUSEACTIVATE` activation.
//!
//! ```text
//! cargo build -p sphere-plugin-host --bins
//! cargo run -p sphere-plugin-host --example plugin_popup_probe -- \
//!     "C:\Program Files\Common Files\VST3\ValhallaDSP\ValhallaRoom.vst3" \
//!     --snapshot probe.bmp --click 240,300 [--studio] [--watch 800]
//! ```

#[cfg(not(windows))]
fn main() {
    eprintln!("plugin_popup_probe is Windows-only");
}

#[cfg(windows)]
fn main() {
    probe::run();
}

#[cfg(windows)]
mod probe {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use windows::core::{w, BOOL, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS, SRCCOPY,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetActiveWindow, GetFocus, SendInput, SetActiveWindow, SetFocus, INPUT, INPUT_0,
        INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EnumWindows,
        GetClassNameW, GetCursorPos, GetWindow, GetWindowRect, GetWindowThreadProcessId, IsChild,
        IsWindowVisible, PeekMessageW, RegisterClassW, SetCursorPos, SetForegroundWindow,
        SetWindowPos, ShowWindow, TranslateMessage, GW_CHILD, HWND_TOPMOST, MSG, PM_REMOVE,
        SWP_SHOWWINDOW, SW_SHOW, WINDOW_EX_STYLE, WM_LBUTTONDOWN, WM_MOUSEACTIVATE,
        WM_PARENTNOTIFY, WM_SETFOCUS, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    use SpherePluginHost::ipc::HostEvent;
    use SpherePluginHost::plugin_host_client::{ClientEvent, PluginHostClient};

    const INSTANCE: &str = "popup-probe-1";
    const ORIGIN: (i32, i32) = (80, 80);
    const CONTENT: (i32, i32) = (1000, 700);

    /// Whether the probe's windows behave like Studio's (see the module docs).
    static STUDIO: AtomicBool = AtomicBool::new(false);

    struct Args {
        plugin: PathBuf,
        snapshot: Option<PathBuf>,
        click: Option<(i32, i32)>,
        /// A second click, in screen pixels (a menu item), after the watch.
        then: Option<(i32, i32)>,
        /// Where to save the editor once the second click has landed.
        after: Option<PathBuf>,
        watch_ms: u64,
    }

    fn point(value: Option<String>) -> Option<(i32, i32)> {
        let value = value?;
        let (x, y) = value.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    }

    fn parse_args() -> Args {
        let mut plugin = None;
        let mut snapshot = None;
        let mut click = None;
        let mut then = None;
        let mut after = None;
        let mut watch_ms = 800;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--snapshot" => snapshot = args.next().map(PathBuf::from),
                "--click" => click = point(args.next()),
                "--then-screen" => then = point(args.next()),
                "--after" => after = args.next().map(PathBuf::from),
                "--watch" => watch_ms = args.next().and_then(|v| v.parse().ok()).unwrap_or(800),
                "--studio" => STUDIO.store(true, Ordering::Relaxed),
                _ => plugin = Some(PathBuf::from(arg)),
            }
        }
        let Some(plugin) = plugin else {
            eprintln!(
                "usage: plugin_popup_probe <plugin.vst3> [--snapshot f.bmp] [--click X,Y] \
                 [--studio] [--watch MS]"
            );
            std::process::exit(2);
        };
        Args {
            plugin,
            snapshot,
            click,
            then,
            after,
            watch_ms,
        }
    }

    pub fn run() {
        let args = parse_args();
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let trace_log = std::env::temp_dir().join("futureboard-focus-trace.log");
        let _ = std::fs::remove_file(&trace_log);
        // Inherited by the host process.
        std::env::set_var("FUTUREBOARD_FOCUS_TRACE", "1");

        let host = SpherePluginHost::plugin_host_client::locate_plugin_host_binary()
            .unwrap_or_else(|error| {
                eprintln!("{error}");
                std::process::exit(2);
            });
        let scanner = host.with_file_name("FutureboardPluginScanner.exe");
        let (class_id, name) = scan_class_id(&scanner, &args.plugin).unwrap_or_else(|error| {
            eprintln!("scan failed: {error}");
            std::process::exit(2);
        });
        println!("plugin {name} class={class_id} host={}", host.display());
        println!("studio-like parent: {}", STUDIO.load(Ordering::Relaxed));

        let mut client = PluginHostClient::spawn_from(&host).expect("spawn host");
        let pid = client.pid();
        let path = args.plugin.to_string_lossy().to_string();
        let _ = client.load_plugin(INSTANCE, &path, &class_id, 48_000, 512, Some("VST3".into()));
        let loaded = wait_for(&client, Duration::from_secs(60), |event| match event {
            HostEvent::PluginLoaded { .. } | HostEvent::PluginAlreadyLoaded { .. } => Some(true),
            HostEvent::PluginLoadFailed { error, .. } => {
                eprintln!("load failed: {error}");
                Some(false)
            }
            _ => None,
        });
        if loaded != Some(true) {
            finish(client);
            return;
        }
        let _ = client.prepare_processing(INSTANCE, 48_000, 512, 2, 2);
        let _ = wait_for(&client, Duration::from_secs(30), |event| {
            matches!(event, HostEvent::ProcessingPrepared { .. }).then_some(())
        });

        let (top, content) = open_parent(&name);
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
            content.0 as u64,
            content.0 as u64,
            CONTENT.0 as u32,
            CONTENT.1 as u32,
            96,
        );
        let attached = wait_for(&client, Duration::from_secs(30), |event| match event {
            HostEvent::EditorAttached { .. } => Some(true),
            HostEvent::EditorAttachFailed { error, .. } => {
                eprintln!("editor attach failed: {error}");
                Some(false)
            }
            _ => None,
        });
        if attached != Some(true) {
            close_parent(top, content);
            finish(client);
            return;
        }
        // Let it draw.
        let _ = wait_for(&client, Duration::from_millis(1500), |_| None::<()>);
        unsafe {
            let child = GetWindow(content, GW_CHILD).unwrap_or_default();
            let mut rect = RECT::default();
            let _ = GetWindowRect(child, &mut rect);
            println!(
                "editor child 0x{:x} '{}' at ({},{}) {}x{}",
                child.0 as usize,
                class_name(child),
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top
            );
        }
        if let Some(snapshot) = args.snapshot.as_ref() {
            match snapshot_client(content, snapshot) {
                Ok(()) => println!("snapshot written to {}", snapshot.display()),
                Err(error) => println!("snapshot failed: {error}"),
            }
        }
        if let Some((x, y)) = args.click {
            click_and_watch(
                &client,
                top,
                content,
                pid,
                (x, y),
                args.watch_ms,
                &trace_log,
            );
        }
        if let Some((x, y)) = args.then {
            let mut restore = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut restore);
                let _ = SetCursorPos(x, y);
            }
            send_button(MOUSEEVENTF_LEFTDOWN);
            let _ = wait_for(&client, Duration::from_millis(60), |_| None::<()>);
            send_button(MOUSEEVENTF_LEFTUP);
            let _ = wait_for(&client, Duration::from_millis(600), |_| None::<()>);
            unsafe {
                let _ = SetCursorPos(restore.x, restore.y);
                let foreground = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
                println!(
                    "after the second click: popups visible={} foreground=0x{:x} (probe window 0x{:x}) focus=0x{:x}",
                    popups(pid).len(),
                    foreground.0 as usize,
                    top.0 as usize,
                    GetFocus().0 as usize
                );
            }
            if let Some(after) = args.after.as_ref() {
                match snapshot_client(content, after) {
                    Ok(()) => println!("after snapshot written to {}", after.display()),
                    Err(error) => println!("after snapshot failed: {error}"),
                }
            }
        }
        let _ = client.close_editor(INSTANCE);
        let _ = wait_for(&client, Duration::from_secs(10), |event| {
            matches!(event, HostEvent::EditorClosed { .. }).then_some(())
        });
        close_parent(top, content);
        finish(client);
    }

    fn finish(mut client: PluginHostClient) {
        let _ = client.unload_plugin(INSTANCE);
        let _ = wait_for(&client, Duration::from_secs(10), |event| {
            matches!(event, HostEvent::PluginUnloaded { .. }).then_some(())
        });
        let _ = client.shutdown();
        let _ = client.wait_for_exit();
    }

    fn click_and_watch(
        client: &PluginHostClient,
        top: HWND,
        content: HWND,
        host_pid: u32,
        (x, y): (i32, i32),
        watch_ms: u64,
        trace_log: &Path,
    ) {
        unsafe {
            let _ = SetForegroundWindow(top);
        }
        let _ = wait_for(client, Duration::from_millis(300), |_| None::<()>);
        let before = popups(host_pid);
        println!("host top-level windows before the click: {}", before.len());
        let mut at = POINT { x, y };
        let mut restore = POINT::default();
        unsafe {
            let _ = ClientToScreen(content, &mut at);
            let _ = GetCursorPos(&mut restore);
            let _ = SetCursorPos(at.x, at.y);
        }
        let trace_from = std::fs::metadata(trace_log).map(|m| m.len()).unwrap_or(0);
        let start = Instant::now();
        send_button(MOUSEEVENTF_LEFTDOWN);
        let mut pressed = true;
        let mut last: Vec<String> = Vec::new();
        while start.elapsed() < Duration::from_millis(watch_ms) {
            // A click, not a hold: release after 80 ms, as a hand does.
            if pressed && start.elapsed() >= Duration::from_millis(80) {
                send_button(MOUSEEVENTF_LEFTUP);
                pressed = false;
                println!("{:>5} ms  button up", start.elapsed().as_millis());
            }
            pump();
            while client.try_recv_event().is_some() {}
            let now: Vec<String> = popups(host_pid)
                .into_iter()
                .filter(|window| !before.contains(window))
                .collect();
            if now != last {
                println!(
                    "{:>5} ms  new visible host windows: {}",
                    start.elapsed().as_millis(),
                    if now.is_empty() {
                        "none".to_string()
                    } else {
                        now.join(" | ")
                    }
                );
                last = now;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if pressed {
            send_button(MOUSEEVENTF_LEFTUP);
        }
        println!(
            "result: {}",
            if last.is_empty() {
                "NO popup visible at the end of the watch (it closed, or never opened)"
            } else {
                "popup still open at the end of the watch"
            }
        );
        unsafe {
            let _ = SetCursorPos(restore.x, restore.y);
        }
        // Close a popup that stayed open before the editor goes.
        let _ = wait_for(client, Duration::from_millis(200), |_| None::<()>);
        if let Ok(text) = std::fs::read(trace_log) {
            let from = (trace_from as usize).min(text.len());
            println!("--- focus trace from the click ---");
            for line in String::from_utf8_lossy(&text[from..]).lines().take(200) {
                println!("{line}");
            }
        }
    }

    fn send_button(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    /// Visible top-level windows of `pid`, described.
    fn popups(pid: u32) -> Vec<String> {
        unsafe extern "system" fn visit(hwnd: HWND, found: LPARAM) -> BOOL {
            unsafe {
                let found = &mut *(found.0 as *mut (u32, Vec<String>));
                let mut owner = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut owner));
                if owner == found.0 && IsWindowVisible(hwnd).as_bool() {
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    found.1.push(format!(
                        "0x{:x} '{}' ({},{} {}x{})",
                        hwnd.0 as usize,
                        class_name(hwnd),
                        rect.left,
                        rect.top,
                        rect.right - rect.left,
                        rect.bottom - rect.top
                    ));
                }
            }
            BOOL(1)
        }
        let mut found = (pid, Vec::new());
        unsafe {
            let _ = EnumWindows(Some(visit), LPARAM(&mut found as *mut _ as isize));
        }
        found.1
    }

    fn class_name(hwnd: HWND) -> String {
        let mut class = [0u16; 128];
        let len = unsafe { GetClassNameW(hwnd, &mut class) } as usize;
        String::from_utf16_lossy(&class[..len])
    }

    /// The content window's client area as a top-down 32-bit BMP, read off
    /// the screen (the probe window is topmost, so that is what it shows).
    fn snapshot_client(content: HWND, path: &Path) -> Result<(), String> {
        unsafe {
            let mut origin = POINT { x: 0, y: 0 };
            let _ = ClientToScreen(content, &mut origin);
            let (w, h) = CONTENT;
            let screen = GetDC(None);
            let memory = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(memory, bitmap.into());
            BitBlt(
                memory,
                0,
                0,
                w,
                h,
                Some(screen),
                origin.x,
                origin.y,
                SRCCOPY,
            )
            .map_err(|e| e.to_string())?;
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            GetDIBits(
                memory,
                bitmap,
                0,
                h as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            );
            SelectObject(memory, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
            ReleaseDC(None, screen);

            let header_len = 14 + 40;
            let mut file = Vec::with_capacity(header_len + pixels.len());
            file.extend_from_slice(b"BM");
            file.extend_from_slice(&((header_len + pixels.len()) as u32).to_le_bytes());
            file.extend_from_slice(&0u32.to_le_bytes());
            file.extend_from_slice(&(header_len as u32).to_le_bytes());
            file.extend_from_slice(&40u32.to_le_bytes());
            file.extend_from_slice(&w.to_le_bytes());
            file.extend_from_slice(&(-h).to_le_bytes());
            file.extend_from_slice(&1u16.to_le_bytes());
            file.extend_from_slice(&32u16.to_le_bytes());
            file.extend_from_slice(&[0u8; 24]);
            file.extend_from_slice(&pixels);
            std::fs::write(path, file).map_err(|e| e.to_string())
        }
    }

    unsafe extern "system" fn top_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // GPUI's handling: activate on every click that reaches the window.
        if msg == WM_MOUSEACTIVATE && STUDIO.load(Ordering::Relaxed) {
            if unsafe { GetActiveWindow() } != hwnd {
                let _ = unsafe { SetActiveWindow(hwnd) };
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    unsafe extern "system" fn content_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Studio's content host: focus goes to the embedded view on a click
        // inside it, and on any focus of the host itself.
        if STUDIO.load(Ordering::Relaxed) {
            let forward = (msg == WM_PARENTNOTIFY && (wparam.0 as u32 & 0xFFFF) == WM_LBUTTONDOWN)
                || msg == WM_SETFOCUS;
            if forward {
                unsafe {
                    if let Ok(child) = GetWindow(hwnd, GW_CHILD) {
                        let focus = GetFocus();
                        let same_thread = !focus.is_invalid()
                            && GetWindowThreadProcessId(focus, None)
                                == GetWindowThreadProcessId(child, None);
                        if focus != child && !IsChild(child, focus).as_bool() && !same_thread {
                            println!("  (studio) content host focuses the view");
                            let _ = SetFocus(Some(child));
                        }
                    }
                }
                if msg == WM_SETFOCUS {
                    return LRESULT(0);
                }
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn open_parent(title: &str) -> (HWND, HWND) {
        let title: Vec<u16> = format!("Popup probe — {title}")
            .encode_utf16()
            .chain([0])
            .collect();
        unsafe {
            let instance = GetModuleHandleW(None).expect("module");
            let top_class = WNDCLASSW {
                lpfnWndProc: Some(top_proc),
                hInstance: instance.into(),
                lpszClassName: w!("FBPopupProbeTop"),
                ..Default::default()
            };
            let content_class = WNDCLASSW {
                lpfnWndProc: Some(content_proc),
                hInstance: instance.into(),
                lpszClassName: w!("FBPopupProbeContent"),
                ..Default::default()
            };
            RegisterClassW(&top_class);
            RegisterClassW(&content_class);
            let top = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("FBPopupProbeTop"),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                ORIGIN.0,
                ORIGIN.1,
                CONTENT.0 + 40,
                CONTENT.1 + 80,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .expect("top window");
            let content = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("FBPopupProbeContent"),
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                0,
                0,
                CONTENT.0,
                CONTENT.1,
                Some(top),
                None,
                Some(instance.into()),
                None,
            )
            .expect("content window");
            let _ = ShowWindow(top, SW_SHOW);
            let _ = SetWindowPos(
                top,
                Some(HWND_TOPMOST),
                ORIGIN.0,
                ORIGIN.1,
                CONTENT.0 + 40,
                CONTENT.1 + 80,
                SWP_SHOWWINDOW,
            );
            (top, content)
        }
    }

    fn close_parent(top: HWND, content: HWND) {
        unsafe {
            let _ = DestroyWindow(content);
            let _ = DestroyWindow(top);
        }
    }

    fn pump() {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn wait_for<T>(
        client: &PluginHostClient,
        timeout: Duration,
        mut pick: impl FnMut(&HostEvent) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            while let Some(event) = client.try_recv_event() {
                match event {
                    ClientEvent::Host(event) => {
                        if let Some(found) = pick(&event) {
                            return Some(found);
                        }
                    }
                    ClientEvent::Disconnected => {
                        eprintln!("host disconnected");
                        return None;
                    }
                }
            }
            pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

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
}
