//! Headless probe for off-screen built-in plugin editor hosting.
//!
//! Opens the same windowless CEF browser the editor window opens, with no GPUI
//! window involved, and reports what actually happens:
//!
//! - whether the browser attaches,
//! - whether frames are painted (and how many),
//! - whether the page's React bridge reaches native (`bridgeReady`).
//!
//! Run it from a directory containing the staged CEF runtime, e.g.:
//!
//! ```text
//! cargo build -p sphere_ui_components --features builtin-plugin-editor \
//!     --example osr_editor_probe
//! cp target/debug/examples/osr_editor_probe out/release/community/linux-x64/
//! cd out/release/community/linux-x64 && FUTUREBOARD_PLUGIN_VIEW_DEBUG=1 ./osr_editor_probe
//! ```
//!
//! `FUTUREBOARD_PLUGIN_VIEW_DEBUG=1` turns on page console forwarding, which is
//! how a JavaScript error inside the editor becomes visible.

#[cfg(feature = "builtin-plugin-editor")]
use sphere_ui_components::components::builtin_plugin_editor as host;

fn main() {
    #[cfg(not(feature = "builtin-plugin-editor"))]
    {
        eprintln!("build with --features builtin-plugin-editor");
    }

    #[cfg(feature = "builtin-plugin-editor")]
    run();
}

#[cfg(feature = "builtin-plugin-editor")]
fn run() {
    use sphere_webview::runtime::ProcessDispatch;

    sphere_webview::runtime::log_process_entry();

    // CEF re-launches this executable for its helpers; they must exit here.
    let mut scheme_app = sphere_webview::scheme::plugin_scheme_app()
        .expect("failed to prepare CEF before process dispatch");
    match sphere_webview::runtime::execute_subprocess(Some(&mut scheme_app))
        .expect("CEF process dispatch failed")
    {
        ProcessDispatch::SubprocessExit(code) => std::process::exit(code),
        ProcessDispatch::BrowserProcess => host::install_process_app(scheme_app)
            .expect("failed to transfer the browser process application"),
    }

    // `FUTUREBOARD_PROBE_PLUGIN=imager` probes another built-in's editor.
    let plugin_id: &'static str = std::env::var("FUTUREBOARD_PROBE_PLUGIN")
        .ok()
        .map(|id| &*Box::leak(id.into_boxed_str()))
        .unwrap_or("rodharerist");
    println!("[probe] availability={}", host::availability(plugin_id));
    if let Err(error) = host::init_at_boot() {
        println!("[probe] FAILED init_at_boot: {error}");
        return;
    }

    let view_id = host::allocate_view_id();
    let rect = host::ViewRect {
        x: 0,
        y: 0,
        width: 972,
        height: 728,
    };
    if let Err(error) = host::open_view(view_id, "probe::instance", plugin_id, 0, rect, 1.0, None) {
        println!("[probe] FAILED open_view: {error}");
        return;
    }

    let origin = host::origin_for_plugin_id(plugin_id).expect("built-in resolves");
    let mut frames = 0;
    let mut inbound = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last_generation = 0;
    let mut input_stage = InputStage::WaitingForBridge;
    let mut seen = InputSeen::default();

    while std::time::Instant::now() < deadline {
        host::pump();
        std::thread::sleep(std::time::Duration::from_millis(16));

        for event in host::take_view_events(view_id) {
            println!("[probe] view_event={event:?}");
        }

        let generation = host::view_frame_generation(view_id);
        if generation != last_generation {
            last_generation = generation;
            frames += 1;
            if frames <= 3 || frames % 60 == 0 {
                let size = host::with_view_frame(view_id, |bytes, w, h| (bytes.len(), w, h));
                println!("[probe] frame #{frames} generation={generation} size={size:?}");
            }
        }

        for raw in host::take_inbound(origin) {
            inbound += 1;
            let body = String::from_utf8_lossy(&raw).into_owned();
            println!("[probe] inbound #{inbound}: {body}");
            if body.contains("futureboard.bridgeReady")
                && input_stage == InputStage::WaitingForBridge
            {
                input_stage = InputStage::InstallListeners;
            }
            seen.record(&body);
        }

        input_stage = drive_input(view_id, input_stage);
    }

    println!("[probe] SUMMARY frames_painted={frames} bridge_messages={inbound}");
    println!(
        "[probe] verdict paint={} bridge={} mouse_move={} mouse_click={} wheel={} key={}",
        verdict(frames > 0),
        verdict(inbound > 0),
        verdict(seen.mouse_move),
        verdict(seen.mouse_click),
        verdict(seen.wheel),
        verdict(seen.key),
    );
    if let Some(position) = seen.click_position {
        println!(
            "[probe] click landed at page coordinates {position:?} (sent {:?})",
            (CLICK_X, CLICK_Y)
        );
    }

    // `FUTUREBOARD_PROBE_FRAME_OUT=<path>` saves the last painted frame as
    // raw BGRA, named `<path>.<width>x<height>.bgra`, for visual inspection.
    if let Some(out) = std::env::var_os("FUTUREBOARD_PROBE_FRAME_OUT") {
        let saved = host::with_view_frame(view_id, |bytes, w, h| {
            let path = format!("{}.{w}x{h}.bgra", out.to_string_lossy());
            std::fs::write(&path, bytes).map(|_| path)
        });
        println!("[probe] frame dump {saved:?}");
    }

    host::close_view(view_id);
    for _ in 0..60 {
        host::pump();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// Where the synthetic click is aimed, in the page's own coordinate space.
/// Any point inside the view works — the page reports back what it received,
/// which is what makes the coordinate mapping checkable rather than assumed.
#[cfg(feature = "builtin-plugin-editor")]
const CLICK_X: i32 = 400;
#[cfg(feature = "builtin-plugin-editor")]
const CLICK_Y: i32 = 300;

#[cfg(feature = "builtin-plugin-editor")]
fn verdict(ok: bool) -> &'static str {
    if ok {
        "OK"
    } else {
        "NONE"
    }
}

/// Which input events the page told us it actually received.
#[cfg(feature = "builtin-plugin-editor")]
#[derive(Default)]
struct InputSeen {
    mouse_move: bool,
    mouse_click: bool,
    wheel: bool,
    key: bool,
    click_position: Option<(i64, i64)>,
}

#[cfg(feature = "builtin-plugin-editor")]
impl InputSeen {
    fn record(&mut self, body: &str) {
        if body.contains("\"probe.mousemove\"") {
            self.mouse_move = true;
        }
        if body.contains("\"probe.wheel\"") {
            self.wheel = true;
        }
        if body.contains("\"probe.keydown\"") {
            self.key = true;
        }
        if body.contains("\"probe.mousedown\"") {
            self.mouse_click = true;
            self.click_position = parse_position(body);
        }
    }
}

/// Pull `"x":<int>,"y":<int>` out of the probe listener's JSON without pulling
/// in a parser — the payload shape is fixed by `LISTENER_SCRIPT` below.
#[cfg(feature = "builtin-plugin-editor")]
fn parse_position(body: &str) -> Option<(i64, i64)> {
    let field = |name: &str| -> Option<i64> {
        let start = body.find(&format!("\"{name}\":"))? + name.len() + 3;
        let rest = &body[start..];
        let end = rest.find([',', '}'])?;
        rest[..end].trim().parse().ok()
    };
    Some((field("x")?, field("y")?))
}

/// Installed in the page once it is up: every input event the page receives is
/// reported straight back through the same bridge endpoint the editor uses.
#[cfg(feature = "builtin-plugin-editor")]
const LISTENER_SCRIPT: &str = r#"
(function () {
  const post = (body) => fetch("__bridge", { method: "POST", body: JSON.stringify(body) });
  addEventListener("mousemove", (e) => post({ type: "probe.mousemove", x: e.clientX, y: e.clientY }), { once: true });
  addEventListener("mousedown", (e) => post({ type: "probe.mousedown", x: e.clientX, y: e.clientY, button: e.button }), { once: true });
  addEventListener("wheel", (e) => post({ type: "probe.wheel", dy: e.deltaY }), { once: true });
  addEventListener("keydown", (e) => post({ type: "probe.keydown", key: e.key }), { once: true });
  post({
    type: "probe.metrics",
    dpr: devicePixelRatio,
    innerWidth: innerWidth,
    innerHeight: innerHeight,
    visualScale: visualViewport ? visualViewport.scale : null,
    webgl: !!document.createElement("canvas").getContext("webgl"),
    canvas2d: !!document.createElement("canvas").getContext("2d"),
  });
  console.log("[cef-diagnostic] probe listeners installed");
})();
"#;

/// Input replay runs one step per pump tick so CEF can deliver each event (and
/// the page's `fetch` back to native can complete) before the next is sent.
#[cfg(feature = "builtin-plugin-editor")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputStage {
    WaitingForBridge,
    InstallListeners,
    /// Pump ticks to wait after installing listeners before replaying input.
    Settle(u8),
    Move,
    Press,
    Release,
    Wheel,
    KeyDown,
    KeyChar,
    KeyUp,
    Done,
}

#[cfg(feature = "builtin-plugin-editor")]
fn drive_input(view_id: host::ViewId, stage: InputStage) -> InputStage {
    use host::{EditorInput, EditorKey, EditorKeyKind, EditorModifiers, EditorMouseButton};

    let held = EditorModifiers {
        left_button: true,
        ..Default::default()
    };
    let none = EditorModifiers::default();

    match stage {
        InputStage::WaitingForBridge | InputStage::Done => return stage,
        InputStage::InstallListeners => {
            host::send_to_view(view_id, LISTENER_SCRIPT);
            // `FUTUREBOARD_PROBE_SCRIPT=<file.js>` runs extra page script once
            // the bridge is up — e.g. to post synthetic analyser frames.
            if let Some(script) = std::env::var_os("FUTUREBOARD_PROBE_SCRIPT")
                .and_then(|path| std::fs::read_to_string(path).ok())
            {
                host::send_to_view(view_id, &script);
            }
            // `execute_javascript` is asynchronous; an event sent on the very
            // next tick can beat the listeners into the document and be missed.
            return InputStage::Settle(6);
        }
        InputStage::Settle(remaining) => {
            return if remaining == 0 {
                InputStage::Move
            } else {
                InputStage::Settle(remaining - 1)
            };
        }
        InputStage::Move => host::send_view_input(
            view_id,
            EditorInput::MouseMove {
                x: CLICK_X,
                y: CLICK_Y,
                modifiers: none,
                leaving: false,
            },
        ),
        InputStage::Press => host::send_view_input(
            view_id,
            EditorInput::MouseButton {
                x: CLICK_X,
                y: CLICK_Y,
                button: EditorMouseButton::Left,
                pressed: true,
                click_count: 1,
                modifiers: held,
            },
        ),
        InputStage::Release => host::send_view_input(
            view_id,
            EditorInput::MouseButton {
                x: CLICK_X,
                y: CLICK_Y,
                button: EditorMouseButton::Left,
                pressed: false,
                click_count: 1,
                modifiers: none,
            },
        ),
        InputStage::Wheel => host::send_view_input(
            view_id,
            EditorInput::MouseWheel {
                x: CLICK_X,
                y: CLICK_Y,
                delta_x: 0,
                delta_y: -40,
                modifiers: none,
            },
        ),
        InputStage::KeyDown => host::send_view_input(
            view_id,
            EditorInput::Key(EditorKey {
                kind: EditorKeyKind::Down,
                windows_key_code: 'A' as i32,
                character: 0,
                modifiers: none,
            }),
        ),
        InputStage::KeyChar => host::send_view_input(
            view_id,
            EditorInput::Key(EditorKey {
                kind: EditorKeyKind::Char,
                windows_key_code: 'a' as i32,
                character: 'a' as u16,
                modifiers: none,
            }),
        ),
        InputStage::KeyUp => host::send_view_input(
            view_id,
            EditorInput::Key(EditorKey {
                kind: EditorKeyKind::Up,
                windows_key_code: 'A' as i32,
                character: 0,
                modifiers: none,
            }),
        ),
    }

    match stage {
        InputStage::Move => InputStage::Press,
        InputStage::Press => InputStage::Release,
        InputStage::Release => InputStage::Wheel,
        InputStage::Wheel => InputStage::KeyDown,
        InputStage::KeyDown => InputStage::KeyChar,
        InputStage::KeyChar => InputStage::KeyUp,
        _ => InputStage::Done,
    }
}
