//! macOS host-owned plug-in editors, end to end against a real spawned host.
//!
//! AppKit has no public cross-process view embedding, so on macOS the plug-in
//! editor is a top-level NSWindow owned by the plug-in host process and the
//! main app passes no parent window (`parent_hwnd = 0`) — the same host-owned
//! model Linux uses. Every format goes the same way: VST3's `IPlugView`, VST2's
//! `effEditOpen` and CLAP's `clap.gui` all attach into a window their own
//! bridge made, under one shared chrome strip. This test drives that path over
//! real IPC, once per format:
//!
//!   spawn host -> Ready -> LoadPlugin -> PluginLoaded
//!               -> OpenEditorWithParentHwnd(parent 0) -> EditorAttached
//!               -> CloseEditor
//!
//! A real plug-in is required, so point each test at one — any it is not given
//! a path for skips rather than fails, because not every machine has all three
//! formats installed:
//!
//!   FUTUREBOARD_TEST_VST3_PATH=/Library/Audio/Plug-Ins/VST3/Some.vst3 \
//!   FUTUREBOARD_TEST_VST2_PATH=/Library/Audio/Plug-Ins/VST/Some.vst \
//!   FUTUREBOARD_TEST_CLAP_PATH=/Library/Audio/Plug-Ins/CLAP/Some.clap \
//!     cargo test -p sphere-plugin-host --features plugin-host-bin \
//!     --test macos_host_owned_editor -- --nocapture --test-threads=1
//!
//! Set `FUTUREBOARD_TEST_VST3_HOLD_MS` to keep each editor on screen after the
//! attach, for visual inspection of a real run.
#![cfg(all(target_os = "macos", feature = "plugin-host-bin"))]

use std::time::{Duration, Instant};

use SpherePluginHost::ipc::{EditorChromePalette, EditorChromeTab, HostCommand, HostEvent};
use SpherePluginHost::plugin_host_client::{ClientEvent, PluginHostClient};
use SpherePluginHost::scan_plugin_bundle;

const INSTANCE_ID: &str = "track1:insert1";

fn wait_for<T>(
    client: &PluginHostClient,
    timeout: Duration,
    mut accept: impl FnMut(HostEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match client.try_recv_event() {
            Some(ClientEvent::Host(event)) => {
                if let Some(value) = accept(event) {
                    return Some(value);
                }
            }
            Some(other) => eprintln!("[test] client event {other:?}"),
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    None
}

fn hold_after_attach() -> Duration {
    let ms = std::env::var("FUTUREBOARD_TEST_VST3_HOLD_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Duration::from_millis(ms)
}

/// VST3: the format the host-owned path landed on first.
#[test]
fn a_vst3_editor_opens_in_a_host_owned_window() {
    open_and_close_a_host_owned_editor("FUTUREBOARD_TEST_VST3_PATH", ".vst3");
}

/// VST2: `effEditOpen` into a container view, and — unlike the other two — an
/// editor that only repaints while the host calls `effEditIdle`.
#[test]
fn a_vst2_editor_opens_in_a_host_owned_window() {
    open_and_close_a_host_owned_editor("FUTUREBOARD_TEST_VST2_PATH", ".vst bundle");
}

/// CLAP: `clap.gui` with the Cocoa window API.
#[test]
fn a_clap_editor_opens_in_a_host_owned_window() {
    open_and_close_a_host_owned_editor("FUTUREBOARD_TEST_CLAP_PATH", ".clap bundle");
}

/// Audio Unit: a Cocoa view from the unit's own view factory, hosted in this
/// process rather than through a module bridge — and carrying the same strip.
///
/// Addressed by component id (`au:<type>:<subtype>:<manufacturer>`), so this one
/// takes an id rather than a path. Apple's stock units have no custom view, so
/// point it at a vendor unit that does.
#[test]
fn an_audio_unit_editor_opens_in_a_host_owned_window() {
    let Ok(component_id) = std::env::var("FUTUREBOARD_TEST_AU_COMPONENT") else {
        eprintln!("skipping: set FUTUREBOARD_TEST_AU_COMPONENT to an installed AU id");
        return;
    };
    eprintln!("[test] plugin={component_id} format=AU");
    drive_host_owned_editor(&component_id, &component_id, &component_id, None);
}

/// Load a plug-in, open its editor with no parent window, push a chrome strip
/// into it, then close it — the whole host-owned lifecycle the studio drives.
fn open_and_close_a_host_owned_editor(path_var: &str, what: &str) {
    let Ok(plugin_path) = std::env::var(path_var) else {
        eprintln!("skipping: set {path_var} to a {what}");
        return;
    };

    let classes = scan_plugin_bundle(std::path::Path::new(&plugin_path))
        .unwrap_or_else(|e| panic!("scan {plugin_path}: {e}"));
    let plugin = classes
        .into_iter()
        .find(|info| info.class_id.is_some())
        .unwrap_or_else(|| panic!("{plugin_path} reported no class id"));
    let class_id = plugin.class_id.clone().expect("class id");
    let format = plugin.format.clone();
    eprintln!(
        "[test] plugin={} vendor={} format={format} class_id={class_id}",
        plugin.name, plugin.vendor
    );
    drive_host_owned_editor(&plugin.name, &plugin_path, &class_id, Some(format));
}

/// Everything from the spawn onwards, which every format shares.
///
/// `format` is `None` for an Audio Unit: it is addressed by component id rather
/// than by a module path, and the host loads it through `LoadAudioUnit` instead.
fn drive_host_owned_editor(
    display_name: &str,
    plugin_path: &str,
    class_id: &str,
    format: Option<String>,
) {
    let mut client = PluginHostClient::spawn_bridge().expect("spawn plugin host");
    assert!(
        wait_for(&client, Duration::from_secs(15), |event| matches!(
            event,
            HostEvent::Ready { .. }
        )
        .then_some(()))
        .is_some(),
        "host never reported Ready"
    );

    match format.clone() {
        // The scanner already knows the format; passing it beats letting the
        // host re-derive one from the path.
        Some(format) => client
            .load_plugin(
                INSTANCE_ID,
                plugin_path,
                class_id,
                48_000,
                512,
                Some(format),
            )
            .expect("send load_plugin"),
        None => client
            .load_au_plugin(INSTANCE_ID, class_id, 48_000, 512, None)
            .expect("send load_au_plugin"),
    }
    let loaded = wait_for(&client, Duration::from_secs(60), |event| match event {
        HostEvent::PluginLoaded { name, .. } | HostEvent::PluginAlreadyLoaded { name, .. } => {
            Some(Ok(name))
        }
        HostEvent::PluginLoadFailed { error, .. } => Some(Err(error)),
        _ => None,
    })
    .expect("host never answered LoadPlugin");
    let loaded = loaded.unwrap_or_else(|error| panic!("plugin load failed: {error}"));
    eprintln!("[test] loaded {loaded}");

    // parent 0: there is no cross-process parent view on macOS. The host must
    // still open its own editor window rather than rejecting the request.
    client
        .open_editor(INSTANCE_ID, plugin_path, class_id, 0, 900, 600, 96)
        .expect("send open_editor");
    let attached = wait_for(&client, Duration::from_secs(60), |event| match event {
        HostEvent::EditorAttached {
            preferred_width,
            preferred_height,
            host_hwnd,
            ..
        } => Some(Ok((preferred_width, preferred_height, host_hwnd))),
        HostEvent::EditorAttachFailed { error, .. } => Some(Err(error)),
        _ => None,
    })
    .expect("host never answered OpenEditorWithParentHwnd");
    let (width, height, host_handle) =
        attached.unwrap_or_else(|error| panic!("editor attach failed: {error}"));
    eprintln!("[test] editor attached size={width}x{height} host_handle=0x{host_handle:x}");

    assert!(
        host_handle != 0,
        "host reported no editor handle, so nothing owns the editor window"
    );
    assert!(
        width >= 32 && height >= 32,
        "editor reported an unusable size {width}x{height}"
    );

    // The chrome strip the studio would push on its first poll after the
    // attach. Everything in it is already formatted and already resolved — the
    // host draws it and decides nothing — so this is exactly the payload the
    // real path sends, with stand-in values.
    client
        .set_editor_chrome(HostCommand::SetEditorChrome {
            plugin_instance_id: INSTANCE_ID.to_string(),
            title: format!("Audio 1 - {display_name} - Insert 1"),
            active: true,
            cpu_label: "12%".to_string(),
            latency_label: "3.2 ms".to_string(),
            preset_label: "Vocal Bus".to_string(),
            presets: vec![
                "Vocal Bus".to_string(),
                "Drum Glue".to_string(),
                "Master Tilt".to_string(),
            ],
            preset_index: Some(0),
            tabs: vec![
                EditorChromeTab {
                    insert_id: INSTANCE_ID.to_string(),
                    display_name: display_name.to_string(),
                    insert_number: 1,
                },
                EditorChromeTab {
                    insert_id: "track1:insert2".to_string(),
                    display_name: "Pro-C 3".to_string(),
                    insert_number: 2,
                },
            ],
            active_tab: INSTANCE_ID.to_string(),
            palette: EditorChromePalette {
                strip_bg: 0x1B1D22FF,
                row_bg: 0x212429FF,
                border: 0x0E1013FF,
                control_bg: 0x16181CFF,
                control_hover: 0x2A2E35FF,
                control_pressed: 0x101216FF,
                accent: 0x4FC9D8FF,
                text_primary: 0xE8EAEEFF,
                text_secondary: 0xB9BDC6FF,
                text_faint: 0x8F949FFF,
            },
        })
        .expect("send set_editor_chrome");

    // Report whatever the host says while the editor is up (EditorClosed when
    // the window is closed by hand, EditorUnresponsive if the pump stalls).
    let hold = hold_after_attach();
    if !hold.is_zero() {
        eprintln!("[test] holding the editor open for {}ms", hold.as_millis());
        let until = Instant::now() + hold;
        while Instant::now() < until {
            match client.try_recv_event() {
                Some(event) => eprintln!("[test] while open: {event:?}"),
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    client.close_editor(INSTANCE_ID).expect("send close_editor");
    client.unload_plugin(INSTANCE_ID).ok();
    client.shutdown().ok();
}
