//! Native plugin editor window (Phase 4 — GPUI-hosted embedding).
//!
//! Architecture:
//! - GPUI owns a borderless external window and draws **only** the shell/header.
//! - A native WS_CHILD host region is created under this window's HWND by the
//!   C++ backend (`native_editor::attach_editor_into_parent`), and the VST3
//!   `IPlugView` is attached into it. The plugin UI is the native view; GPUI
//!   never draws plugin content.
//! - On Windows the native app sets `GPUI_DISABLE_DIRECT_COMPOSITION=1` at boot.
//! - VST3 UI is hosted in an **owned tool window** (`WS_POPUP|WS_EX_TOOLWINDOW`)
//!   aligned to the content region below the GPUI titlebar (default). Set
//!   `FUTUREBOARD_PLUGIN_EDITOR_MODE=child` to force in-client `WS_CHILD` embed.
//! - The GPUI shell uses an opaque background; the tool window carries the plugin UI.
//! - No audio-thread interaction: attach/resize/detach run on the UI thread.
//! - Editor failure never crashes — a GPUI fallback panel is shown instead.
//!
//! The bullets above describe the *Windows* mechanism, which is the one this
//! window was written against. There are two backends — see
//! [`crate::components::plugin_editor_backend::EditorBackendKind`]:
//!
//! - **Windows** embeds the plug-in's view in this window, through a content
//!   child HWND handed to the plug-in host process.
//! - **macOS and Linux** have no public way to reparent a view across a process
//!   boundary, so the plug-in's view is attached inside a top-level window the
//!   *host process* owns. This window then has no plug-in region at all: it
//!   creates no content child, pushes no geometry, and does not grow to the
//!   plug-in's size. It carries the chrome that cannot be drawn over a plug-in's
//!   surface, and it goes when the host reports `EditorClosed`.
//!
//! Every branch between the two reads `embeds_in_editor_window()`, and none
//! reads the target directly, so the Windows path stays the code it always was.
//!
//! The old C++ NanoVG/D3D top-level window is no longer used on this path.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    div, point, px, size, App, AppContext, Bounds, Context, FocusHandle, InteractiveElement,
    IntoElement, ParentElement, Pixels, Point, Render, Size, StatefulInteractiveElement, Styled,
    Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::plugin_content_host::{ContentChildHwnd, ContentRect};
use crate::components::plugin_editor_backend::{
    host_region_wait, local_native_view_ready, EditorBackendKind, HostRegionWait,
    LocalEditorCompatibility,
};
use crate::components::plugin_editor_chrome::{
    open_preset_menu as open_preset_menu_window, preset_menu_size, render_chrome_tools,
    render_tab_strip, PluginEditorAction, PluginEditorChrome, PluginEditorTab, PresetMenuWindow,
    TAB_STRIP_H,
};
use crate::components::title_bar::TITLEBAR_HEIGHT;
use crate::layout::plugin_bridge_runtime::SharedPluginBridgeRuntime;
use crate::theme::{self, Colors};
use SpherePluginHost::editor_quirk::{match_quirk, PluginEditorQuirk};
use SpherePluginHost::ipc::HostEvent;
use SpherePluginHost::native_editor::PluginEditorPresentationMode;
use SpherePluginHost::plugin_host_client::{
    plugin_host_bridge_enabled, ClientEvent, PluginHostClient,
};

/// Physical-pixel host region under the GPUI window. (Local mirror of the
/// backend's region struct — the editor is now driven by the DirectAudio runtime
/// instance, not SpherePluginHost.)
#[derive(Debug, Clone, Copy, Default)]
struct EmbedRegion {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Map the DirectAudio embed host-kind code (0 = WS_CHILD, 1 = owned tool window,
/// 2 = detached top-level) to the shared presentation-mode enum. Exactly one
/// mode is active per editor.
fn presentation_mode_from_host_kind(kind: i32) -> PluginEditorPresentationMode {
    match kind {
        0 => PluginEditorPresentationMode::ChildHwndEmbed,
        2 => PluginEditorPresentationMode::DetachedNativeWindow,
        _ => PluginEditorPresentationMode::OwnedToolWindowFallback,
    }
}

/// State for the separated-process editor backend. Present only when host-process
/// ownership is selected and the host spawned successfully. The main app owns
/// the window + the content child HWND; the host process owns the VST3 view.
struct HostEditorBackend {
    /// One host process per open editor (simplest lifecycle + crash isolation;
    /// a shared host is a later optimization). Drop shuts it down.
    client: Option<PluginHostClient>,
    shared: Option<SharedPluginBridgeRuntime>,
    /// Main-app-owned `WS_CHILD` content HWND the host attaches the view into.
    content: Option<ContentChildHwnd>,
    plugin_path: String,
    class_id: String,
    /// Captured from `HostEvent::Ready` for diagnostics.
    host_pid: Option<u32>,
    /// Last content rect pushed to the host (dedup resize spam).
    last_region: Option<ContentRect>,
}

/// Spawn the bridge host and complete a Ping/Pong handshake. Returns `None` on
/// any failure. The caller's mandatory bridge gate ensures we NEVER fall back to
/// the in-process editor path unless the explicit legacy override is enabled.
/// `[plugin-bridge]` diagnostics are emitted throughout.
fn build_host_backend(
    processor: Option<&DirectAudio::Vst3RuntimeProcessor>,
    _display_name: &str,
    shared: Option<SharedPluginBridgeRuntime>,
) -> Option<HostEditorBackend> {
    if let Some(shared) = shared {
        let host_pid = shared.lock().ok().and_then(|runtime| runtime.host_pid());
        return Some(HostEditorBackend {
            client: None,
            shared: Some(shared),
            content: None,
            plugin_path: String::new(),
            class_id: String::new(),
            host_pid,
            last_region: None,
        });
    }
    let processor = processor?;
    let plugin_path = processor.plugin_path().map(|s| s.to_string());
    let class_id = processor.class_id().map(|s| s.to_string());

    let mut client = match PluginHostClient::spawn_bridge() {
        Ok(c) => c,            // spawn_bridge logged current_exe/resolved/exists/spawned
        Err(_) => return None, // spawn_bridge already logged spawn_failed
    };

    // Liveness handshake before any editor command.
    eprintln!("[plugin-bridge] sending Ping");
    if let Err(e) = client.ping() {
        eprintln!("[plugin-bridge] spawn_failed error=ping send: {e}");
        return None;
    }
    let mut host_pid = Some(client.pid());
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut ponged = false;
    while Instant::now() < deadline {
        match client.try_recv_event() {
            Some(ClientEvent::Host(HostEvent::Pong { pid })) => {
                host_pid = Some(pid);
                ponged = true;
                break;
            }
            Some(ClientEvent::Host(HostEvent::Ready { pid, .. })) => {
                host_pid = Some(pid); // startup Ready; keep waiting for Pong
            }
            Some(ClientEvent::Disconnected) => {
                eprintln!("[plugin-bridge] spawn_failed error=host disconnected during handshake");
                return None;
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    if !ponged {
        eprintln!("[plugin-bridge] spawn_failed error=handshake timeout (no Pong)");
        return None;
    }
    eprintln!("[plugin-bridge] received Pong");

    // The bridge is live. If plugin identity is missing we still go through the
    // bridge (skeleton OpenEditor) — we must never touch the in-process path.
    Some(HostEditorBackend {
        client: Some(client),
        shared: None,
        content: None,
        plugin_path: plugin_path.unwrap_or_default(),
        class_id: class_id.unwrap_or_default(),
        host_pid,
        last_region: None,
    })
}

/// Logical-pixel height of the chrome row under the titlebar.
///
/// A row of its own rather than controls squeezed into the titlebar: the
/// titlebar has a plug-in's name to show and window buttons to keep reachable,
/// and plug-in names are long. This is the strip the host owns above the
/// plug-in's surface.
const CHROME_H: f32 = 26.0;

/// Logical-pixel height reserved above the plug-in: titlebar, tabs, chrome row.
const HEADER_H: f32 = TITLEBAR_HEIGHT + TAB_STRIP_H + CHROME_H;

/// How long a click on the preset trigger is ignored after the list dismissed
/// itself.
///
/// Clicking the trigger while the list is open deactivates the list first,
/// which closes it; the click then arrives with the trigger's `menu_open`
/// captured from a frame that may or may not have landed in between. Without
/// this guard the same gesture closes the list on one machine and
/// closes-then-reopens it on another.
const PRESET_MENU_REOPEN_GUARD: Duration = Duration::from_millis(250);

pub const EDITOR_WINDOW_WIDTH: f32 = 820.0;
pub const EDITOR_WINDOW_HEIGHT: f32 = 560.0;
pub const EDITOR_WINDOW_MIN_WIDTH: f32 = 360.0;
pub const EDITOR_WINDOW_MIN_HEIGHT: f32 = 200.0;

/// How many ~32 ms ticks we wait for the GPUI window to produce a valid native
/// handle + non-zero content bounds before giving up and surfacing a visible
/// error. ~5 s — generous, but never an infinite silent spin.
const MAX_WAIT_TICKS: u32 = 150;

/// Resolved once: this is asked from the editor tick, several times a frame.
fn plugin_view_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some())
}

/// Explicit lifecycle state for the embedded editor. The UI renders a distinct
/// surface for each state so a blank panel never appears unless we are actually
/// `Attached` with a live native child.
#[derive(Clone, Debug, PartialEq)]
enum PluginEditorStatus {
    /// Window just opened; native handle/bounds not yet probed.
    Opening,
    /// Native parent HWND or content bounds not ready — retrying.
    WaitingForHostHandle,
    /// Bounds are ready; about to create the native child + attach.
    Attaching,
    /// IPlugView::attached returned ok but no visible plug-in UI yet. We poll
    /// `embed_has_visible_ui` at the Phase-6 milestones below; the editor is
    /// promoted to `Attached` as soon as a visible UI appears. WebView/CEF
    /// WebView/CEF-backed editors regularly land here for hundreds of ms.
    ProbingReady {
        mode: PluginEditorPresentationMode,
        probe_index: u8,
    },
    /// Native editor attached and visible, via exactly one presentation mode.
    Attached(PluginEditorPresentationMode),
    /// Attach failed — fallback panel with Retry / Close.
    Failed(String),
    /// This platform has no native-view embedding at all, so there is nothing
    /// to retry. Distinct from [`Self::Failed`] precisely so the panel can drop
    /// the Retry button: offering one for a permanent gap is a control that
    /// cannot do what it says, and it sent people looking for a broken plug-in
    /// or a timing bug that was never there.
    Unsupported(String),
}

/// Phase 6: delays (ms) between visible-UI re-checks after a successful
/// `IPlugView::attached`. Cap at the last entry — anything still blank past
/// that turns into a surfaced failure.
const READY_PROBE_DELAYS_MS: &[u64] = &[100, 500, 1000, 3000, 5000];
const DEFAULT_PLUGIN_EDITOR_CONTENT_SIZE: (i32, i32) = (900, 600);

/// DPI reported alongside a host-owned editor's requested size.
///
/// That size is in the plug-in's own logical units (points on macOS), so the
/// DPI that describes it is the unscaled one. The host records the pair in its
/// open log; a backing-scaled number beside a logical size would only make the
/// log lie about which space the request was in.
const HOST_OWNED_EDITOR_DPI: u32 = 96;

/// Logical width of the shell window when the plug-in's editor is a window of
/// the host process's own.
///
/// Wide enough for the chrome row — bypass, preset stepper, Save, CPU and
/// latency — plus a tab with a real plug-in name on it. There is no plug-in
/// surface in this window to size around, so nothing else argues for a width.
const HOST_OWNED_SHELL_WIDTH: f32 = 480.0;

/// Logical height below the chrome for the line that says where the plug-in's
/// window went.
///
/// Derived rather than chosen: it is whatever is left once the chrome has taken
/// its share of the window's own minimum height. The shell would be clamped up
/// to that minimum anyway, so a smaller target would only be a number that the
/// window never takes — and it comfortably fits the two lines the panel draws.
const HOST_OWNED_SHELL_BODY_H: f32 = EDITOR_WINDOW_MIN_HEIGHT - HEADER_H;
const MIN_PLUGIN_EDITOR_CONTENT_SIZE: i32 = 160;
const MAX_PLUGIN_EDITOR_PREFERRED_SIZE: i32 = 4096;

/// See [`PluginEditorWindow::unsupported_status_for`].
fn unsupported_status(reason: &LocalEditorCompatibility) -> PluginEditorStatus {
    PluginEditorStatus::Unsupported(reason.message().to_string())
}

pub struct PluginEditorWindow {
    pub track_id: String,
    pub insert_id: String,
    display_name: String,
    /// Clone of the live runtime VST3 instance for this insert. The editor view
    /// is created from THIS instance's controller — never a new one — so GUI
    /// edits drive the actual audio processor. Holding the clone keeps the C++
    /// instance alive while the editor is open.
    processor: Option<DirectAudio::Vst3RuntimeProcessor>,
    /// Editor handle from the embed attach; `None` until first attach.
    embed_handle: Option<u64>,
    /// The window the plug-in's view is parked in, on the host-owned path.
    ///
    /// GPUI owns it: this window creates it, positions it under the header,
    /// destroys it, and is the only thing that resizes it. The bridge is handed
    /// the handle and does nothing to it but `attached()` / `removed()`.
    view_content: Option<ContentChildHwnd>,
    status: PluginEditorStatus,
    /// Number of waiting ticks elapsed (reset on retry).
    wait_ticks: u32,
    /// Whether a deferred re-render tick is already queued (avoids spawning a
    /// timer on every render frame while waiting).
    tick_scheduled: bool,
    /// Logged the "host region mounted" line once bounds first went non-zero.
    host_mounted_logged: bool,
    last_region: Option<(i32, i32, i32, i32)>,
    /// Forced content size used for initial bridge auto-size. Cleared after the
    /// first successful auto-size so manual user resize controls the session.
    editor_content_size: Option<(i32, i32)>,
    host_preferred_size: Option<(i32, i32)>,
    host_auto_size_applied: bool,
    host_auto_size_settled: bool,
    /// Editor quirk resolved from the plug-in path + name at construction.
    /// Drives the delayed-ready ramp and informs failure messaging.
    quirk: PluginEditorQuirk,
    /// `Some` when the bridge is active and the host spawned. When `None` and
    /// `bridge_required` is false, the explicit legacy in-process path runs.
    host: Option<HostEditorBackend>,
    /// True for the default mandatory external bridge. Hard gate: while set,
    /// the in-process editor path is NEVER used — if `host` is `None` the window
    /// surfaces a failure instead of silently embedding in-process.
    bridge_required: bool,
    /// What the titlebar strip shows, as of the studio's last refresh.
    ///
    /// Pushed in rather than read here: the insert, the engine and the preset
    /// store all belong to the studio, and a window that went looking for them
    /// itself would be a second place each of them is decided.
    chrome: PluginEditorChrome,
    /// Chrome controls the user pressed, waiting for the studio to apply them.
    ///
    /// Filled by this window's own strip on the embedding backend, and by
    /// `EditorChromeAction` from the host on a host-owned one. The studio
    /// drains the same queue either way, so a control means one thing on every
    /// platform.
    chrome_actions: Vec<PluginEditorAction>,
    /// The last chrome pushed to the host, so an unchanged one is not resent.
    ///
    /// `None` on the embedding backend, which never pushes at all.
    pushed_chrome: Option<SpherePluginHost::ipc::HostCommand>,
    /// Whether a failure has already put this window on screen.
    ///
    /// Only a host-owned shell is ever off screen to begin with, and only the
    /// first failure shows it — activating on every tick would take focus back
    /// from whatever the user moved on to.
    failure_surfaced: bool,
    /// The open preset list, which is a window of its own.
    ///
    /// Window state, not the studio's: which menu is open is nobody else's
    /// business, and it closes the moment a preset is picked.
    ///
    /// A window rather than an element because the region a dropdown from the
    /// chrome row falls into belongs to the plug-in's native child, and
    /// `WS_CLIPCHILDREN` removes that rectangle from this window's visible
    /// region. Drawn here it was built every frame and never once visible; see
    /// [`PresetMenuWindow`] for the full trace.
    preset_menu: Option<WindowHandle<PresetMenuWindow>>,
    /// Set while a preset list has been asked for but not yet opened.
    ///
    /// `open_window` may not be called from a UI callback, so the request and
    /// the opening are two different turns of the event loop; this keeps a
    /// double click from queueing two of them.
    preset_menu_requested: bool,
    /// When the list last closed, for [`PRESET_MENU_REOPEN_GUARD`].
    preset_menu_dismissed_at: Option<Instant>,
    /// Set when the list closed, cleared by the next draw.
    ///
    /// Closing the list returns Win32 activation to this window, but GPUI's own
    /// focus went with the list's window and nothing gives it back on its own —
    /// the editor would keep drawing while the keyboard pointed at a window that
    /// no longer exists. `render` is the one place with a `Window` to focus.
    preset_menu_refocus: bool,
    /// The preset trigger's window-space rect, written by the chrome row on
    /// every prepaint.
    ///
    /// The list is a separate window, so it has to be placed against a real
    /// measurement rather than a constant that stops matching the row the first
    /// time a control is added to it. Shared with the element rather than
    /// returned by it because `render` builds the row and the click that opens
    /// the list happens later — the same measure-in-render, act-later shape the
    /// docked ARA panel uses.
    preset_anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// This window's client rect in screen coordinates, recorded during the
    /// last draw, plus the viewport and scale that go with it.
    ///
    /// The preset list is a separate window and has to be placed in screen
    /// coordinates and clamped to this window's own client area, but the click
    /// that opens it arrives with no `Window` to ask. `render` is the one place
    /// that has one, so it leaves the answers here.
    window_bounds: Bounds<Pixels>,
    window_viewport: Size<Pixels>,
    window_scale: f32,
    /// This window's native handle, so the preset list can be owned by it.
    window_hwnd: Option<u64>,
    /// Every plug-in open on this channel, in slot order.
    ///
    /// One window per channel: its inserts are a chain, and the tab strip is how
    /// the user moves along it. Only the active tab's view is attached — a
    /// plug-in editor is a cross-process native window, and holding several of
    /// them live behind one another buys nothing anybody can see.
    tabs: Vec<PluginEditorTab>,
    focus_handle: FocusHandle,
}

impl PluginEditorWindow {
    pub(crate) fn new(
        track_id: String,
        insert_id: String,
        display_name: String,
        processor: Option<DirectAudio::Vst3RuntimeProcessor>,
        shared_bridge: Option<SharedPluginBridgeRuntime>,
        in_process: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let quirk = processor
            .as_ref()
            .and_then(|processor| {
                processor
                    .plugin_path()
                    .map(|p| match_quirk(std::path::Path::new(p), Some(&display_name), None))
            })
            .unwrap_or_default();
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view] open requested plugin=\"{}\" track={} insert={} quirk={} delayed_ready={} sta={} extra_pump={} plugin_webview_based={}",
                display_name,
                track_id,
                insert_id,
                quirk.name,
                quirk.delayed_ready_check,
                quirk.requires_sta_com,
                quirk.extra_message_pump,
                quirk.plugin_webview_based,
            );
        }
        // ARA instances live in this process by construction: their host
        // callbacks read project state directly, so they cannot be hosted behind
        // the bridge. For those the in-process editor is the only path, and it is
        // opened deliberately rather than through the legacy escape hatch.
        let display_name_for_chrome = display_name.clone();
        let bridge_required = plugin_host_bridge_enabled() && !in_process;
        let host = if bridge_required {
            build_host_backend(processor.as_ref(), &display_name, shared_bridge)
        } else {
            None
        };
        // Bridge mandatory but host unavailable → fail visibly; never fall back
        // to the in-process path.
        let status = if bridge_required && host.is_none() {
            eprintln!("[plugin-view] editor open failed because mandatory bridge is unavailable");
            PluginEditorStatus::Failed(
                "External PluginHost bridge is mandatory, but the FutureboardPluginHostX64 \
                 process could not be started. The in-process editor is disabled unless \
                 FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS=1 is set."
                    .to_string(),
            )
        } else {
            PluginEditorStatus::Opening
        };
        Self {
            track_id,
            insert_id,
            display_name,
            processor,
            embed_handle: None,
            view_content: None,
            chrome: PluginEditorChrome {
                plugin_name: display_name_for_chrome,
                ..PluginEditorChrome::default()
            },
            chrome_actions: Vec::new(),
            pushed_chrome: None,
            failure_surfaced: false,
            preset_menu: None,
            preset_menu_requested: false,
            preset_menu_dismissed_at: None,
            preset_menu_refocus: false,
            preset_anchor: Rc::new(Cell::new(None)),
            window_bounds: Bounds {
                origin: point(px(0.0), px(0.0)),
                size: size(px(0.0), px(0.0)),
            },
            window_viewport: size(px(0.0), px(0.0)),
            window_scale: 1.0,
            window_hwnd: None,
            tabs: Vec::new(),
            status,
            wait_ticks: 0,
            tick_scheduled: false,
            host_mounted_logged: false,
            last_region: None,
            editor_content_size: None,
            host_preferred_size: None,
            host_auto_size_applied: false,
            host_auto_size_settled: false,
            quirk,
            host,
            bridge_required,
            focus_handle: cx.focus_handle(),
        }
    }

    fn editor_id(&self) -> String {
        format!("{}::{}", self.track_id, self.insert_id)
    }

    fn valid_preferred_size(width: u32, height: u32) -> Option<(i32, i32)> {
        let width = i32::try_from(width).ok()?;
        let height = i32::try_from(height).ok()?;
        if width >= MIN_PLUGIN_EDITOR_CONTENT_SIZE
            && height >= MIN_PLUGIN_EDITOR_CONTENT_SIZE
            && width <= MAX_PLUGIN_EDITOR_PREFERRED_SIZE
            && height <= MAX_PLUGIN_EDITOR_PREFERRED_SIZE
        {
            Some((width, height))
        } else {
            None
        }
    }

    fn preferred_size_or_default(width: u32, height: u32) -> (i32, i32) {
        Self::valid_preferred_size(width, height).unwrap_or(DEFAULT_PLUGIN_EDITOR_CONTENT_SIZE)
    }

    /// Physical-pixel host region under the GPUI window: full client width, from
    /// just below the header to the bottom. Win32 child coords are physical, so
    /// logical sizes are scaled by the window DPI factor.
    fn host_region_for(&self, window: &Window) -> EmbedRegion {
        let scale = window.scale_factor().max(0.5);
        let viewport = window.viewport_size();
        let w: f32 = viewport.width.into();
        let h: f32 = viewport.height.into();
        let header_px = HEADER_H * scale;
        // A fixed-size editor gets exactly its own size and the window is sized
        // around it. A resizable one follows the window, so the user dragging
        // the frame actually reaches the plug-in instead of being overridden by
        // the size it happened to open at.
        // On the bridged path there is no local view to ask, and the region is
        // released by `maybe_release_initial_preferred_size` once the user
        // resizes away from the plug-in's preferred size — so it keeps using the
        // recorded content size, exactly as before.
        let fixed = match self.processor.as_ref() {
            Some(processor) => !processor.view_can_resize(),
            None => true,
        };
        if fixed {
            if let Some((content_w, content_h)) = self.editor_content_size {
                return EmbedRegion {
                    x: 0,
                    y: header_px.round() as i32,
                    width: content_w.max(1),
                    height: content_h.max(1),
                };
            }
        }
        EmbedRegion {
            x: 0,
            y: header_px.round() as i32,
            width: (w * scale).round().max(1.0) as i32,
            height: ((h * scale) - header_px).round().max(1.0) as i32,
        }
    }

    /// Extract the native window handle (HWND on Windows) from the GPUI window
    /// via the `raw-window-handle` trait. `None` on unsupported platforms or if
    /// the handle is unavailable.
    #[cfg(target_os = "windows")]
    fn native_parent_handle(window: &Window) -> Option<u64> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // NB: `Window::window_handle()` (inherent) returns gpui's AnyWindowHandle;
        // the raw handle is the same-named trait method — call it qualified.
        let handle = HasWindowHandle::window_handle(window).ok()?;
        match handle.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get() as u64),
            _ => None,
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn native_parent_handle(_window: &Window) -> Option<u64> {
        None
    }

    /// Where the preset list goes, in screen coordinates.
    ///
    /// Resolved against this window's own client rect first, so the list is
    /// clamped to the editor surface it belongs to and flips above the trigger
    /// rather than running off a short editor, then translated by the window
    /// origin because the list is a window of its own.
    fn resolve_preset_menu_bounds(&self) -> Option<Bounds<Pixels>> {
        let viewport_w: f32 = self.window_viewport.width.into();
        let viewport_h: f32 = self.window_viewport.height.into();
        if viewport_w < 1.0 || viewport_h < 1.0 {
            // Nothing has been drawn yet, so there is no client rect to clamp
            // against and no measured trigger either.
            return None;
        }
        // Until the row has been prepainted once the chrome row itself is the
        // anchor: still inside the editor, still under the header, and derived
        // from the layout rather than from a constant nobody maintains.
        let anchor = self.preset_anchor.get().unwrap_or(Bounds {
            origin: point(px(0.0), px(TITLEBAR_HEIGHT + TAB_STRIP_H)),
            size: size(self.window_viewport.width, px(CHROME_H)),
        });
        let resolved = crate::overlay::resolve_popup_placement(
            anchor,
            preset_menu_size(self.chrome.presets.len()),
            Bounds {
                origin: point(px(0.0), px(0.0)),
                size: self.window_viewport,
            },
            crate::overlay::PopupPlacementOptions {
                preferred_side: crate::overlay::PopupSide::Bottom,
                alignment: crate::overlay::PopupAlignment::Start,
                viewport_margin: px(theme::space::TIGHT),
                gap: px(theme::space::NONE),
            },
        );
        Some(Bounds {
            origin: self.window_bounds.origin + resolved.origin,
            size: resolved.size,
        })
    }

    /// Asks for the preset list; it opens on a later turn of the event loop.
    ///
    /// Never opened from the click itself. `open_window` reaches the platform,
    /// and creating a window dispatches synchronous messages that re-enter GPUI;
    /// `layout/plugin_ops.rs` states the rule for the editor window and this is
    /// the same platform call from the same kind of callback. A spawned task is
    /// past the frame entirely, which no `defer` from inside an update can
    /// promise.
    fn request_preset_menu(&mut self, cx: &mut Context<Self>) {
        if self.preset_menu.is_some() || self.preset_menu_requested {
            return;
        }
        if self
            .preset_menu_dismissed_at
            .is_some_and(|at| at.elapsed() < PRESET_MENU_REOPEN_GUARD)
        {
            return;
        }
        self.preset_menu_requested = true;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(1)).await;
            let _ = this.update(cx, |editor, cx| {
                if !editor.preset_menu_requested {
                    return;
                }
                editor.preset_menu_requested = false;
                editor.open_preset_menu(cx);
            });
        })
        .detach();
    }

    /// Opens the preset list under the chrome row's preset trigger.
    fn open_preset_menu(&mut self, cx: &mut Context<Self>) {
        self.close_preset_menu(cx);
        let Some(bounds) = self.resolve_preset_menu_bounds() else {
            eprintln!(
                "[plugin-preset-menu] no client rect yet for editor_id={}; not opening the list",
                self.editor_id()
            );
            return;
        };
        let this = cx.entity().downgrade();
        self.preset_menu = open_preset_menu_window(
            bounds,
            self.window_bounds,
            self.window_hwnd,
            self.window_scale,
            self.chrome.clone(),
            move |action, cx| {
                let delivered = this.update(cx, |editor, cx| {
                    match action {
                        // The list closes its own window; the editor only
                        // forgets the handle, so no callback the list is running
                        // ever re-enters the list's own entity.
                        PluginEditorAction::TogglePresetMenu(false) => {
                            editor.preset_menu = None;
                            editor.preset_menu_requested = false;
                            editor.preset_menu_dismissed_at = Some(Instant::now());
                            editor.preset_menu_refocus = true;
                        }
                        PluginEditorAction::TogglePresetMenu(true) => {}
                        action => editor.chrome_actions.push(action),
                    }
                    cx.notify();
                });
                if delivered.is_err() {
                    eprintln!(
                        "[plugin-preset-menu] dropped a preset action: the editor window is gone"
                    );
                }
            },
            cx,
        );
        if self.preset_menu.is_none() {
            // `open_preset_menu` already said why. Said again here because from
            // the user's side this is "the preset button does nothing".
            eprintln!(
                "[plugin-preset-menu] the preset list did not open for editor_id={}",
                self.editor_id()
            );
        }
    }

    /// Closes the preset list if it is open.
    ///
    /// `pub(crate)` because the studio closes the whole editor tab from
    /// `layout::plugin_editor_chrome_ops`, and a list of a plug-in that is going
    /// away must not outlive it.
    pub(crate) fn close_preset_menu(&mut self, cx: &mut Context<Self>) {
        self.preset_menu_requested = false;
        if let Some(handle) = self.preset_menu.take() {
            self.preset_menu_dismissed_at = Some(Instant::now());
            self.preset_menu_refocus = true;
            let _ = handle.update(cx, |_menu, window, _cx| window.remove_window());
        }
    }

    /// Everything `drive` does — creating the content child window, sending
    /// `OpenEditor`, resizing this window — dispatches Win32 messages
    /// synchronously, and any of those can re-enter GPUI. Run from inside
    /// `render` that re-entry lands in the middle of a frame and dereferences
    /// the element arena the frame is still building: "attempted to dereference
    /// an ArenaRef after its Arena was cleared". So `render` only ever records
    /// what it saw and calls this; the work itself happens here, in an update
    /// that owns the window rather than one that is drawing it.
    fn schedule_tick(&mut self, cx: &mut Context<Self>) {
        if self.tick_scheduled {
            return;
        }
        self.tick_scheduled = true;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(16)).await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.tick_scheduled = false;
                this.drive(window, cx);
            });
        })
        .detach();
    }

    /// No parent handle. On Windows that is a frame the window was not ready
    /// on yet and worth waiting through; off Windows it is the permanent gap —
    /// `native_parent_handle` returns `None` unconditionally, so no number of
    /// ticks will ever change it, and spinning `MAX_WAIT_TICKS` only delays a
    /// message that then blames a timeout for a platform that was never
    /// implemented.
    /// The status a refused editor ends in.
    ///
    /// [`PluginEditorStatus::Unsupported`], never [`PluginEditorStatus::Failed`]
    /// — the difference between them is the Retry button, and offering one for
    /// a permanent refusal is a control that cannot do what it says.
    fn unsupported_status_for(reason: &LocalEditorCompatibility) -> PluginEditorStatus {
        unsupported_status(reason)
    }

    /// The host region is not ready. Whether that is worth waiting through is
    /// the backend's question, not a shared one.
    ///
    /// The remote backend (Windows) really does get its parent handle a frame
    /// or two late, so it waits. The local backend (macOS) is not waiting for a
    /// handle at all — it has no foreign handle in its design — so a miss there
    /// means the in-process view path is not built yet, and saying "never
    /// became ready" would invent a timeout for a mechanism that was never
    /// going to run.
    fn note_host_region_not_ready(&mut self, cx: &mut Context<Self>) {
        let backend = EditorBackendKind::current();
        match host_region_wait(backend, local_native_view_ready()) {
            HostRegionWait::KeepWaiting => {
                // Named per backend so the log says what *this* platform is
                // short of, not what another platform would have been.
                self.note_waiting(backend.readiness().label(), cx);
            }
            HostRegionWait::GiveUp(reason) => {
                self.note_editor_unsupported(backend, reason, cx);
            }
        }
    }

    /// Report a permanent refusal with its reason. Never a panic, never a
    /// fabricated timeout, and never a fallback window opened behind the
    /// user's back.
    fn note_editor_unsupported(
        &mut self,
        backend: EditorBackendKind,
        reason: LocalEditorCompatibility,
        cx: &mut Context<Self>,
    ) {
        eprintln!(
            "[PluginEditorBackend] unsupported plugin='{}' insert={} backend={} reason={}",
            self.display_name,
            self.insert_id,
            backend.label(),
            reason.log_reason()
        );
        self.status = unsupported_status(&reason);
        cx.notify();
    }

    fn note_waiting(&mut self, reason: &str, cx: &mut Context<Self>) {
        self.wait_ticks += 1;
        if self.wait_ticks > MAX_WAIT_TICKS {
            let msg = format!("host region never became ready ({reason})");
            if plugin_view_debug() {
                eprintln!(
                    "[plugin-view] attach failed error={msg} editor_id={}",
                    self.editor_id()
                );
            }
            self.status = PluginEditorStatus::Failed(msg);
            cx.notify();
            return;
        }
        if self.status != PluginEditorStatus::WaitingForHostHandle {
            self.status = PluginEditorStatus::WaitingForHostHandle;
        }
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view] waiting ({reason}) editor_id={} tick={}/{MAX_WAIT_TICKS}",
                self.editor_id(),
                self.wait_ticks
            );
        }
        self.schedule_tick(cx);
    }

    /// Drive the attach lifecycle. Called at the top of every render (which has
    /// both the live `Window` and `Context`). Never blocks; transitions through
    /// explicit states and defers via `schedule_tick` until bounds are ready.
    fn drive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host.is_some() {
            self.drive_host(window, cx);
            return;
        }
        let Some(processor) = self.processor.as_ref() else {
            self.status = PluginEditorStatus::Failed(
                "Plugin editor requires a runtime processor when the external bridge is disabled."
                    .to_string(),
            );
            cx.notify();
            return;
        };
        // Hard gate: mandatory bridge but no host process → never touch the
        // in-process editor path. Stay in a surfaced failure (set in `new`).
        if self.bridge_required {
            if !matches!(self.status, PluginEditorStatus::Failed(_)) {
                self.status = PluginEditorStatus::Failed(
                    "External PluginHost bridge is mandatory but host process is unavailable; \
                     in-process editor is disabled unless FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS=1."
                        .to_string(),
                );
                cx.notify();
            }
            return;
        }
        match self.status.clone() {
            PluginEditorStatus::Attached(PluginEditorPresentationMode::DetachedNativeWindow) => {
                // The plug-in owns a standalone window; the GPUI shell only
                // watches for the user closing that window (WM_CLOSE) or the
                // native window vanishing, then tears the editor down.
                if self.embed_handle.is_some()
                    && (processor.embed_take_user_close() || !processor.embed_is_valid())
                {
                    if plugin_view_debug() {
                        eprintln!(
                            "[plugin-view] detached window closed editor_id={} → removing shell",
                            self.editor_id()
                        );
                    }
                    window.remove_window();
                }
                return;
            }
            PluginEditorStatus::Attached(_) => {
                self.sync_region(window);
                return;
            }
            // Nothing to retry and nothing to wait for: stop driving.
            PluginEditorStatus::Failed(_) | PluginEditorStatus::Unsupported(_) => return,
            PluginEditorStatus::Attaching => {
                self.perform_attach(window, cx);
                return;
            }
            PluginEditorStatus::ProbingReady { .. } => {
                // The probe scheduler advances the state — keep the host region
                // in sync (parent moves still translate to the embed) while we
                // wait for the WebView/CEF children to materialize.
                self.sync_region(window);
                return;
            }
            PluginEditorStatus::Opening | PluginEditorStatus::WaitingForHostHandle => {}
        }

        // A host-owned backend attaches into a top-level window the bridge
        // creates, so there is no parent to require and no region to reserve —
        // the same shape as the bridged path above, minus the process boundary.
        if !EditorBackendKind::current().embeds_in_editor_window() {
            self.wait_ticks = 0;
            self.status = PluginEditorStatus::Attaching;
            self.schedule_tick(cx);
            cx.notify();
            return;
        }

        // Phase 4/6: require a valid native parent handle before attaching.
        let Some(parent) = Self::native_parent_handle(window) else {
            self.note_host_region_not_ready(cx);
            return;
        };
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view] top hwnd=0x{parent:x} editor_id={}",
                self.editor_id()
            );
        }

        // Phase 7: require real (>0) content bounds before attaching.
        let region = self.host_region_for(window);
        if region.width <= 0 || region.height <= 0 {
            self.note_waiting("host bounds not ready (0x0)", cx);
            return;
        }

        if !self.host_mounted_logged {
            self.host_mounted_logged = true;
            if plugin_view_debug() {
                eprintln!(
                    "[plugin-view] host region mounted bounds={{x:{},y:{},w:{},h:{}}} editor_id={}",
                    region.x,
                    region.y,
                    region.width,
                    region.height,
                    self.editor_id()
                );
            }
        }

        // Bounds are ready — move to a visible Attaching state, then let the
        // next tick perform the (potentially blocking) attach so the UI can
        // first paint "Attaching plugin editor…".
        self.wait_ticks = 0;
        self.status = PluginEditorStatus::Attaching;
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view] attach requested editor_id={} parent=0x{parent:x} size={}x{}",
                self.editor_id(),
                region.width,
                region.height
            );
        }
        self.schedule_tick(cx);
        cx.notify();
    }

    fn perform_attach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(processor) = self.processor.clone() else {
            self.status =
                PluginEditorStatus::Failed("missing in-process runtime processor".to_string());
            cx.notify();
            return;
        };
        if !EditorBackendKind::current().embeds_in_editor_window() {
            self.perform_host_owned_attach(processor, window, cx);
            return;
        }
        let Some(parent) = Self::native_parent_handle(window) else {
            // Lost the handle between scheduling and now — go back to waiting.
            self.status = PluginEditorStatus::WaitingForHostHandle;
            self.note_waiting("native parent handle lost before attach", cx);
            return;
        };
        let region = self.host_region_for(window);
        if region.width <= 0 || region.height <= 0 {
            self.status = PluginEditorStatus::WaitingForHostHandle;
            self.note_waiting("host bounds not ready before attach", cx);
            return;
        }
        // GPUI owns the window the view goes into. The bridge is handed the
        // handle and does nothing to it but attach and detach the plug-in's
        // view — there is no C++ shell, titlebar, or window procedure on this
        // path, so this window is the single owner of the editor's geometry.
        let content_rect = ContentRect {
            x: region.x,
            y: region.y,
            width: region.width.max(1),
            height: region.height.max(1),
        };
        let content = match self.view_content.as_ref() {
            Some(content) if content.is_valid() => {
                content.set_bounds(content_rect);
                None
            }
            _ => match ContentChildHwnd::create(parent, content_rect) {
                Some(content) => Some(content),
                None => {
                    self.status = PluginEditorStatus::Failed(
                        "could not create the editor surface".to_string(),
                    );
                    cx.notify();
                    return;
                }
            },
        };
        if let Some(content) = content {
            self.view_content = Some(content);
        }
        let content_hwnd = self
            .view_content
            .as_ref()
            .map(ContentChildHwnd::hwnd)
            .unwrap_or(0);
        // Attach the editor view of the EXISTING runtime instance — never create
        // a new VST3 component/controller for the editor.
        match processor.view_attach(content_hwnd, (region.width, region.height)) {
            Some((preferred_w, preferred_h)) => {
                self.embed_handle = Some(content_hwnd);
                // One presentation mode remains on this path: the view lives in
                // a child window this process owns. The tool-window and
                // detached-native-window shells were the C++ host's, and it is
                // no longer in the loop.
                let mode = PluginEditorPresentationMode::ChildHwndEmbed;
                self.editor_content_size = Some((preferred_w, preferred_h));
                let applied_region = self.apply_native_auto_size(window).unwrap_or(region);
                self.last_region = Some((
                    applied_region.x,
                    applied_region.y,
                    applied_region.width,
                    applied_region.height,
                ));
                self.grant_view_size(applied_region);
                let visible = processor.embed_has_visible_ui();
                if visible {
                    self.status = PluginEditorStatus::Attached(mode);
                    if plugin_view_debug() {
                        eprintln!(
                            "[plugin-view] attach ok editor_id={} content=0x{content_hwnd:x} parent=0x{parent:x} mode={mode:?} visible=immediate (reused runtime instance)",
                            self.editor_id()
                        );
                    }
                } else {
                    // Phase 6: enter the delayed-ready probe. WebView/CEF
                    // WebView/CEF-backed editors routinely take 100–3000 ms
                    // before any visible child window materializes — failing now
                    // would always lose them.
                    self.status = PluginEditorStatus::ProbingReady {
                        mode,
                        probe_index: 0,
                    };
                    if plugin_view_debug() {
                        eprintln!(
                            "[plugin-view] attach ok editor_id={} content=0x{content_hwnd:x} parent=0x{parent:x} mode={mode:?} visible=deferred (probing ready)",
                            self.editor_id()
                        );
                    }
                    self.schedule_ready_probe(0, cx);
                }
            }
            None => {
                let err = self
                    .processor
                    .as_ref()
                    .and_then(|processor| processor.last_error())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| {
                        "failed to attach editor to runtime plugin instance \
                         (no ready VST3 processor for this insert)"
                            .to_string()
                    });
                if plugin_view_debug() {
                    eprintln!(
                        "[plugin-view] attach failed error={err} editor_id={}",
                        self.editor_id()
                    );
                }
                self.status = PluginEditorStatus::Failed(err);
            }
        }
        cx.notify();
    }

    /// Attach an in-process editor into a window the bridge owns.
    ///
    /// The legacy in-process path's arm of the same split the bridged path
    /// makes: `view_attach(0, …)` reaches `sphere_daux_vst3_embed_editor`,
    /// which on macOS and Linux creates its own top-level window and attaches
    /// the view there. The size handed over is the plug-in's own logical size,
    /// not this window's physical region — that region belongs to a surface
    /// this path never creates.
    fn perform_host_owned_attach(
        &mut self,
        processor: DirectAudio::Vst3RuntimeProcessor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (width, height) = self
            .editor_content_size
            .unwrap_or(DEFAULT_PLUGIN_EDITOR_CONTENT_SIZE);
        let width = width.max(MIN_PLUGIN_EDITOR_CONTENT_SIZE);
        let height = height.max(MIN_PLUGIN_EDITOR_CONTENT_SIZE);
        match processor.view_attach(0, (width, height)) {
            Some((preferred_w, preferred_h)) => {
                // The bridge's opaque editor handle, not a window this process
                // owns — `Drop`, `retry` and the user-close poll read it only as
                // "there is an editor to detach", which is what it means here.
                // Floored at 1 rather than trusted: a zero would make `Drop`
                // skip the detach and leave the plug-in's window behind after
                // this shell is gone.
                self.embed_handle = Some(processor.embed_attach_hwnd().max(1));
                if let Some(size) = u32::try_from(preferred_w)
                    .ok()
                    .zip(u32::try_from(preferred_h).ok())
                    .and_then(|(w, h)| Self::valid_preferred_size(w, h))
                {
                    self.editor_content_size = Some(size);
                }
                self.apply_host_owned_shell_size(window);
                let mode = EditorBackendKind::current().presentation();
                self.status = PluginEditorStatus::Attached(mode);
                eprintln!(
                    "[plugin-view] attach ok editor_id={} mode={mode:?} host_owned=true \
                     size={preferred_w}x{preferred_h} (reused runtime instance)",
                    self.editor_id()
                );
                cx.notify();
            }
            None => {
                let err = processor
                    .last_error()
                    .unwrap_or_else(|| "the plug-in's editor could not be opened".to_string());
                eprintln!(
                    "[plugin-view] attach failed error={err} editor_id={}",
                    self.editor_id()
                );
                self.status = PluginEditorStatus::Failed(err);
                cx.notify();
            }
        }
    }

    /// Tells the view the size this window is actually giving it, and moves the
    /// child window it lives in to match.
    ///
    /// The plug-in never learns a size the window did not apply: the region is
    /// run through the VST3 size contract first, so a fixed-size editor keeps
    /// its own size and a resizable one takes what it is offered.
    fn grant_view_size(&mut self, region: EmbedRegion) {
        let Some(processor) = self.processor.as_ref() else {
            return;
        };
        if let Some(content) = self.view_content.as_ref() {
            content.set_bounds(ContentRect {
                x: region.x,
                y: region.y,
                width: region.width.max(1),
                height: region.height.max(1),
            });
        }
        let (width, height) = processor.view_constrain(region.width, region.height);
        processor.view_set_size(width, height);
    }

    fn apply_native_auto_size(&mut self, window: &mut Window) -> Option<EmbedRegion> {
        let (content_w, content_h) = self
            .processor
            .as_ref()?
            .view_size()
            .or(self.editor_content_size)?;
        self.editor_content_size = Some((content_w, content_h));

        let scale = window.scale_factor().max(0.5);
        let shell_w = (content_w as f32 / scale).max(EDITOR_WINDOW_MIN_WIDTH);
        let shell_h = ((content_h as f32 / scale) + HEADER_H).max(EDITOR_WINDOW_MIN_HEIGHT);
        window.resize(size(px(shell_w), px(shell_h)));

        let region = self.host_region_for(window);
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view] auto_size plugin=\"{}\" shell={:.0}x{:.0} content={}x{} editor_id={}",
                self.display_name,
                shell_w,
                shell_h,
                region.width,
                region.height,
                self.editor_id()
            );
        }
        Some(region)
    }

    /// User-initiated retry from the failure panel: tear down any partial state
    /// and restart the lifecycle from `Opening`.
    fn retry(&mut self, cx: &mut Context<Self>) {
        if self.embed_handle.take().is_some() {
            // Detach the editor view only — the runtime processor keeps running.
            // Order matters: the plug-in lets go of the child window, and only
            // then is the child destroyed.
            if let Some(processor) = self.processor.as_ref() {
                processor.view_detach();
            }
            self.view_content = None;
        }
        self.status = PluginEditorStatus::Opening;
        self.wait_ticks = 0;
        self.host_mounted_logged = false;
        self.last_region = None;
        self.editor_content_size = None;
        self.host_preferred_size = None;
        self.host_auto_size_applied = false;
        self.host_auto_size_settled = false;
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view] retry requested editor_id={}",
                self.editor_id()
            );
        }
        cx.notify();
    }

    /// Phase 6: schedule a deferred visible-UI re-check. WebView/CEF editors
    /// WebView/CEF-backed editors routinely take 100–3000 ms after
    /// `IPlugView::attached()` before any visible child window materializes.
    /// We poll at the Phase-6 milestones (100/500/1000/3000/5000 ms); the
    /// first probe to see visible UI promotes the editor to `Attached`. The
    /// final probe surfaces a failure if everything is still blank.
    fn schedule_ready_probe(&mut self, probe_index: u8, cx: &mut Context<Self>) {
        let idx = probe_index as usize;
        let Some(&delay_ms) = READY_PROBE_DELAYS_MS.get(idx) else {
            // Out of range — caller should have promoted by now.
            return;
        };
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(delay_ms)).await;
            let _ = this.update(cx, |this, cx| {
                this.on_ready_probe(probe_index, cx);
            });
        })
        .detach();
    }

    fn on_ready_probe(&mut self, probe_index: u8, cx: &mut Context<Self>) {
        // Only act if we are still in ProbingReady for *this* probe sequence —
        // a retry or close may have moved the state under us.
        let PluginEditorStatus::ProbingReady {
            mode,
            probe_index: current,
        } = self.status.clone()
        else {
            return;
        };
        if current != probe_index {
            return;
        }
        let Some(processor) = self.processor.as_ref() else {
            self.status =
                PluginEditorStatus::Failed("missing in-process runtime processor".to_string());
            cx.notify();
            return;
        };
        // Nothing to nudge: the view is a child of a window this process owns
        // and moves with it. The probe is now purely "has the plug-in put
        // anything on screen yet", which is the only question it ever asked.
        let visible = processor.embed_has_visible_ui();
        let is_last = probe_index as usize + 1 >= READY_PROBE_DELAYS_MS.len();
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view] ready-probe editor_id={} step={}/{} delay_ms={} visible={}",
                self.editor_id(),
                probe_index as usize + 1,
                READY_PROBE_DELAYS_MS.len(),
                READY_PROBE_DELAYS_MS[probe_index as usize],
                visible
            );
        }
        if visible {
            self.status = PluginEditorStatus::Attached(mode);
            cx.notify();
            return;
        }
        if is_last {
            // Cap reached and still blank — detach + show fallback panel.
            if self.embed_handle.take().is_some() {
                processor.view_detach();
            }
            self.view_content = None;
            let msg = format!(
                "Editor attached but no visible WebView/editor window appeared \
                 after {} ms. The plug-in may host a Chromium/CEF view that did \
                 not initialize. Try Retry, switch to the Owned Tool Window \
                 fallback, or check the plug-in's runtime requirements.",
                READY_PROBE_DELAYS_MS.last().copied().unwrap_or(5000)
            );
            self.status = PluginEditorStatus::Failed(msg);
            cx.notify();
            return;
        }
        // Schedule the next probe in the ramp.
        self.status = PluginEditorStatus::ProbingReady {
            mode,
            probe_index: probe_index + 1,
        };
        self.schedule_ready_probe(probe_index + 1, cx);
    }

    fn sync_region(&mut self, window: &mut Window) {
        if !matches!(
            self.status,
            PluginEditorStatus::Attached(_) | PluginEditorStatus::ProbingReady { .. }
        ) {
            return;
        }
        // Cloned, not borrowed: this window resizes itself and its child in
        // response to what the plug-in reports, and both need `&mut self`.
        let Some(processor) = self.processor.clone() else {
            return;
        };
        // VST2 editors repaint and animate only while the host calls
        // effEditIdle; VST3 and CLAP run their own timers and this is a no-op
        // for them. Runs before the embed check so an editor that is still
        // materializing its child window keeps getting ticked.
        processor.view_idle();
        if self.embed_handle.is_none() || !processor.embed_is_valid() {
            return;
        }
        // A plug-in that wants to change size asks; it does not act. The request
        // is recorded by the bridge and answered here, where this window is free
        // to resize itself first and only then tell the view what it got.
        if let Some(requested) = processor.view_take_resize_request() {
            self.editor_content_size = Some(requested);
        }
        if let Some(plugin_size) = self.editor_content_size {
            if self.host_preferred_size != Some(plugin_size) {
                self.host_preferred_size = Some(plugin_size);
                self.editor_content_size = Some(plugin_size);
                let scale = window.scale_factor().max(0.5);
                let shell_w = (plugin_size.0 as f32 / scale).max(EDITOR_WINDOW_MIN_WIDTH);
                let shell_h =
                    ((plugin_size.1 as f32 / scale) + HEADER_H).max(EDITOR_WINDOW_MIN_HEIGHT);
                window.resize(size(px(shell_w), px(shell_h)));
                if plugin_view_debug() {
                    eprintln!(
                        "[plugin-view] auto_size plugin=\"{}\" shell={:.0}x{:.0} content={}x{} editor_id={}",
                        self.display_name,
                        shell_w,
                        shell_h,
                        plugin_size.0,
                        plugin_size.1,
                        self.editor_id()
                    );
                }
            }
        }
        let region = self.host_region_for(window);
        let tuple = (region.x, region.y, region.width, region.height);
        // Only push an explicit resize when our client-relative region actually
        // changed (Part D — ignore resize events if the rect is unchanged).
        if self.last_region != Some(tuple) {
            self.last_region = Some(tuple);
            if plugin_view_debug() {
                eprintln!(
                    "[plugin-view] resize host bounds={{x:{},y:{},w:{},h:{}}} editor_id={}",
                    region.x,
                    region.y,
                    region.width,
                    region.height,
                    self.editor_id()
                );
            }
            self.grant_view_size(region);
        }
        // Nothing per-frame beyond this point. The view is a child of a window
        // this process owns, so a parent window move carries it along with no
        // work at all — where the C++ shell had to recompute a screen rect
        // every frame to stay glued to the host.
    }

    // --- Host-process editor path (gated; in-process path above is untouched) ---

    /// Drive the separated-process editor lifecycle. Mirrors `drive` but the
    /// VST3 view lives in `FutureboardPluginHostX64.exe`: the main app creates a
    /// content child HWND under its GPUI window and hands the handle to the host
    /// over IPC. Attach is event-driven (`HostEvent::EditorAttached`).
    fn drive_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 1. In CLIENT mode this editor solely owns the IPC channel, so it drains
        //    its own events here. In SHARED-bridge mode the shared runtime queue
        //    has exactly ONE drain — `StudioLayout::poll_plugin_bridge_runtime` —
        //    which routes editor-targeted events to us via `ingest_host_event`.
        //    Draining the shared queue here too would race that poll and silently
        //    swallow `EditorAttached`/`EditorPreferredSize`, leaving us stuck on
        //    "Loading" (spec Part 2/5/6).
        let mut events = Vec::new();
        if let Some(host) = self.host.as_ref() {
            if host.shared.is_none() {
                if let Some(client) = host.client.as_ref() {
                    while let Some(ev) = client.try_recv_event() {
                        events.push(ev);
                    }
                }
            }
        }
        for ev in events {
            self.on_host_event(ev, cx);
        }
        self.apply_host_preferred_size(window);

        match self.status.clone() {
            PluginEditorStatus::Attached(PluginEditorPresentationMode::DetachedNativeWindow) => {
                // The plug-in's window belongs to the host process, which sizes
                // it and reports back. Pushing this shell's region at it would
                // fight the plug-in over a window this process does not own.
                //
                // The user closing that window arrives as `EditorClosed`, which
                // the studio routes here and then drops this shell with it, so
                // there is nothing to poll for — only the light tick that
                // notices the host going away.
                self.schedule_tick(cx);
                return;
            }
            // A host-owned shell is never shown, so a failure would otherwise
            // be silent: the editor simply would not appear, with nothing
            // anywhere saying why. Showing the window is the whole point of
            // having kept it.
            PluginEditorStatus::Failed(_) | PluginEditorStatus::Unsupported(_)
                if !self.failure_surfaced =>
            {
                self.failure_surfaced = true;
                eprintln!(
                    "[plugin-editor-window] surfacing the shell to report a failure editor_id={}",
                    self.editor_id()
                );
                window.activate_window();
                cx.notify();
                return;
            }
            PluginEditorStatus::Attached(_) => {
                self.sync_host_region(window);
                // Keep a light tick so a host crash (EditorDisconnected) is
                // noticed promptly even with no user interaction.
                self.schedule_tick(cx);
                return;
            }
            // Nothing to retry and nothing to wait for: stop driving.
            PluginEditorStatus::Failed(_) | PluginEditorStatus::Unsupported(_) => return,
            PluginEditorStatus::Attaching | PluginEditorStatus::ProbingReady { .. } => {
                // Waiting for EditorAttached / EditorAttachFailed.
                self.schedule_tick(cx);
                return;
            }
            PluginEditorStatus::Opening | PluginEditorStatus::WaitingForHostHandle => {}
        }

        // 2. A host-owned backend has no parent handle and no content child in
        //    this process: the plug-in's window belongs to the host, so the
        //    steps below — top HWND, content child, region — have nothing to
        //    operate on. Asking for them anyway is what refused every editor on
        //    macOS.
        if !EditorBackendKind::current().embeds_in_editor_window() {
            self.request_host_owned_editor(window, cx);
            return;
        }

        // 3. Need a valid GPUI top HWND before we can parent a content child.
        let Some(top) = Self::native_parent_handle(window) else {
            self.note_host_region_not_ready(cx);
            return;
        };
        let region = self.host_region_for(window);
        if region.width <= 0 || region.height <= 0 {
            self.note_waiting("host bounds not ready (host mode)", cx);
            return;
        }
        let rect = ContentRect {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        };
        let dpi = (window.scale_factor().max(0.5) * 96.0).round() as u32;
        let id = self.editor_id();

        // 4. Create the main-app-owned content child HWND (content != top).
        let Some(content) = ContentChildHwnd::create(top, rect) else {
            self.status =
                PluginEditorStatus::Failed("failed to create content child HWND".to_string());
            cx.notify();
            return;
        };
        let content_hwnd = content.hwnd();
        eprintln!(
            "[plugin-view][host] top_hwnd=0x{top:x} content_hwnd=0x{content_hwnd:x} editor_id={id}"
        );
        eprintln!("[plugin-editor-window] ownership=main_owned");
        eprintln!("[plugin-editor-window] shell_hwnd=0x{top:x}");
        eprintln!("[plugin-editor-window] content_hwnd=0x{content_hwnd:x}");
        eprintln!("[plugin-editor-window] content_parent=shell_hwnd");

        // 5. Send OpenEditorWithParentHwnd to the host process.
        let (path, class_id) = {
            let host = self.host.as_ref().unwrap();
            (host.plugin_path.clone(), host.class_id.clone())
        };
        {
            let host = self.host.as_mut().unwrap();
            host.content = Some(content);
            let pid = host
                .host_pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "pending".to_string());
            eprintln!(
                "[plugin-bridge] sending OpenEditorWithParentHwnd instance={id} hwnd=0x{content_hwnd:x}"
            );
            let open_result = if let Some(shared) = host.shared.as_ref() {
                shared
                    .lock()
                    .map_err(|_| "bridge runtime lock poisoned".to_string())
                    .and_then(|mut runtime| {
                        runtime
                            .open_editor_with_parent(
                                self.insert_id.clone(),
                                content_hwnd,
                                rect.width as u32,
                                rect.height as u32,
                                dpi,
                            )
                            .map_err(|e| e.to_string())
                    })
            } else if let Some(client) = host.client.as_mut() {
                client
                    .open_editor(
                        id.clone(),
                        path,
                        class_id,
                        content_hwnd,
                        rect.width as u32,
                        rect.height as u32,
                        dpi,
                    )
                    .map_err(|e| e.to_string())
            } else {
                Err("bridge host client unavailable".to_string())
            };
            match open_result {
                Ok(()) => {
                    host.last_region = Some(rect);
                    eprintln!(
                        "[plugin-view][host] OpenEditorWithParentHwnd sent editor_id={id} \
                         content_hwnd=0x{content_hwnd:x} host_pid={pid} size={}x{} dpi={dpi}",
                        rect.width, rect.height
                    );
                }
                Err(e) => {
                    self.status =
                        PluginEditorStatus::Failed(format!("send OpenEditor failed: {e}"));
                    cx.notify();
                    return;
                }
            }
        }
        self.wait_ticks = 0;
        self.status = PluginEditorStatus::Attaching;
        self.schedule_tick(cx);
        cx.notify();
    }

    /// Ask the host process to open the plug-in's editor in a window it owns.
    ///
    /// macOS and Linux both land here. Neither has public cross-process view
    /// reparenting, so the plug-in's `NSView`/`GtkWidget` is created and
    /// attached inside the host process, in a top-level window that process
    /// owns — `sphere_daux_vst3_embed_editor` has taken that branch since the
    /// Linux path landed, and the host brings up and pumps `NSApplication` for
    /// it. Nothing is embedded in this window, so there is no parent handle to
    /// wait for, no content child to create, and no region to push.
    ///
    /// `parent_hwnd` is deliberately `0`. The host treats it as an optional
    /// owner/DPI reference for its own top-level window and never as a
    /// `SetParent` target (`schedule_unified_editor_attach`), and this process
    /// has no handle whose meaning would survive the boundary anyway.
    fn request_host_owned_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The plug-in's own coordinate space — points on macOS, logical pixels
        // on Linux — never this window's physical region. The region is the
        // shell's geometry and has nothing to do with a window in another
        // process; sending it would open a Retina editor at twice its size.
        let (width, height) = self
            .editor_content_size
            .unwrap_or(DEFAULT_PLUGIN_EDITOR_CONTENT_SIZE);
        let width = width.max(MIN_PLUGIN_EDITOR_CONTENT_SIZE) as u32;
        let height = height.max(MIN_PLUGIN_EDITOR_CONTENT_SIZE) as u32;
        // Both sizes above are already logical, so the DPI that goes with them
        // is the unscaled one. The host logs it against the requested size, and
        // a scaled number beside a logical size would only misdescribe it.
        let dpi = HOST_OWNED_EDITOR_DPI;
        let id = self.editor_id();
        let backend = EditorBackendKind::current();

        let (path, class_id) = {
            let Some(host) = self.host.as_ref() else {
                return;
            };
            (host.plugin_path.clone(), host.class_id.clone())
        };
        let Some(host) = self.host.as_mut() else {
            return;
        };
        // No content child on this path, and no region either: the plug-in's
        // window sizes itself and reports back through `EditorContentResize`.
        host.content = None;
        host.last_region = None;
        eprintln!(
            "[plugin-editor-window] ownership=host_owned backend={} editor_id={id}",
            backend.label()
        );
        eprintln!(
            "[plugin-bridge] sending OpenEditor (host-owned window) instance={id} size={width}x{height} dpi={dpi}"
        );
        let open_result = if let Some(shared) = host.shared.as_ref() {
            shared
                .lock()
                .map_err(|_| "bridge runtime lock poisoned".to_string())
                .and_then(|mut runtime| {
                    runtime
                        .open_editor_with_parent(self.insert_id.clone(), 0, width, height, dpi)
                        .map_err(|e| e.to_string())
                })
        } else if let Some(client) = host.client.as_mut() {
            client
                .open_editor(id.clone(), path, class_id, 0, width, height, dpi)
                .map_err(|e| e.to_string())
        } else {
            Err("bridge host client unavailable".to_string())
        };
        if let Err(e) = open_result {
            self.status = PluginEditorStatus::Failed(format!("send OpenEditor failed: {e}"));
            cx.notify();
            return;
        }
        // The shell has no plug-in surface to make room for, so it takes the
        // size of its own chrome now rather than after the plug-in answers.
        self.apply_host_owned_shell_size(window);
        self.wait_ticks = 0;
        self.status = PluginEditorStatus::Attaching;
        self.schedule_tick(cx);
        cx.notify();
    }

    /// Bring the editor to the front when the user opens it again.
    ///
    /// On the embedding backend the studio activates this window and that is
    /// the whole gesture. On a host-owned backend this window is not on screen
    /// at all, so activating it raises nothing — the editor the user is asking
    /// for is a window in the host process, and only the host can raise it. A
    /// repeated open is how it is told to: the host answers an open for an
    /// instance it already has attached by focusing it.
    pub(crate) fn focus_editor_surface(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if EditorBackendKind::current().embeds_in_editor_window() {
            window.activate_window();
            return;
        }
        match self.status {
            // Already reported, and already on screen. Bring it forward again:
            // the user asked for this editor and the message is the answer.
            PluginEditorStatus::Failed(_) | PluginEditorStatus::Unsupported(_) => {
                window.activate_window();
            }
            // Attached: ask the host again. It answers an open for an instance
            // it already has by focusing that window, which is the window the
            // user is actually pointing at.
            PluginEditorStatus::Attached(_) => self.request_host_owned_editor(window, cx),
            // Still opening. Showing the shell now would put an empty frame on
            // screen a moment before the plug-in's own window arrives beside
            // it, which is the pair of windows this backend exists to avoid.
            _ => {}
        }
    }

    /// Hand the chrome to whoever is drawing it.
    ///
    /// On the embedding backend that is this window and there is nothing to
    /// send: GPUI draws the strip from `self.chrome` on the next frame. On a
    /// host-owned backend the strip is an AppKit view beside the plug-in in
    /// the host process, so the same state goes over the bridge — already
    /// formatted and with the theme resolved, because the host has neither the
    /// preset files nor the theme store to work either out.
    ///
    /// Skipped when nothing changed. This is called from every chrome refresh,
    /// which runs on the studio's poll, and a repaint per poll for a CPU
    /// reading that landed on the same percent is exactly the churn the GPUI
    /// path already avoids.
    fn push_host_chrome(&mut self) {
        if EditorBackendKind::current().embeds_in_editor_window() {
            return;
        }
        let command = SpherePluginHost::ipc::HostCommand::SetEditorChrome {
            plugin_instance_id: self.insert_id.clone(),
            title: self.window_title(),
            active: self.chrome.active,
            cpu_label: self.chrome.cpu_label(),
            latency_label: self.chrome.latency_label(),
            preset_label: self.chrome.preset_label(),
            presets: self.chrome.presets.clone(),
            preset_index: self.chrome.preset_index.map(|index| index as u32),
            tabs: self
                .tabs
                .iter()
                .map(|tab| SpherePluginHost::ipc::EditorChromeTab {
                    insert_id: tab.insert_id.clone(),
                    display_name: tab.display_name.clone(),
                    insert_number: tab.insert_number as u32,
                })
                .collect(),
            active_tab: self.insert_id.clone(),
            palette: crate::components::plugin_editor_chrome::resolve_chrome_palette(),
        };
        if self.pushed_chrome.as_ref() == Some(&command) {
            return;
        }
        let Some(host) = self.host.as_mut() else {
            return;
        };
        if let Some(shared) = host.shared.as_ref() {
            let Ok(mut runtime) = shared.lock() else {
                return;
            };
            runtime.set_editor_chrome(command.clone());
        } else if let Some(client) = host.client.as_mut() {
            if client.set_editor_chrome(command.clone()).is_err() {
                return;
            }
        } else {
            return;
        }
        self.pushed_chrome = Some(command);
    }

    /// Size the shell for a host-owned editor: its chrome, and nothing else.
    ///
    /// The plug-in's window is the host process's and carries the plug-in's own
    /// size; this window carries only the controls that cannot be drawn over a
    /// plug-in's surface. Growing it to the plug-in's size would reserve a
    /// region that nothing will ever paint into.
    fn apply_host_owned_shell_size(&mut self, window: &mut Window) {
        if self.host_auto_size_applied {
            return;
        }
        self.host_auto_size_applied = true;
        self.host_auto_size_settled = true;
        let target_w = HOST_OWNED_SHELL_WIDTH.max(EDITOR_WINDOW_MIN_WIDTH);
        let target_h = (HEADER_H + HOST_OWNED_SHELL_BODY_H).max(EDITOR_WINDOW_MIN_HEIGHT);
        let viewport = window.viewport_size();
        let current_w: f32 = viewport.width.into();
        let current_h: f32 = viewport.height.into();
        if (current_w - target_w).abs() <= 1.0 && (current_h - target_h).abs() <= 1.0 {
            return;
        }
        eprintln!(
            "[plugin-editor-window] host_owned_shell_size {target_w:.0}x{target_h:.0} \
             (the plug-in's window is the host process's)"
        );
        window.resize(size(px(target_w), px(target_h)));
    }

    /// Refreshes what the titlebar strip shows.
    ///
    /// Cheap to call every poll: an unchanged chrome notifies nothing, so the
    /// window does not repaint for a CPU reading that landed on the same
    /// rounded percent.
    pub(crate) fn set_chrome(&mut self, chrome: PluginEditorChrome, cx: &mut Context<Self>) {
        if self.chrome == chrome {
            return;
        }
        // The open list holds a snapshot of this chrome. Rather than push a new
        // one into a window the user is already pointing at — which would move
        // the row under the pointer — the list closes when the set of presets it
        // is showing stops being the truth. It is reopened with the fresh list.
        if self.chrome.presets != chrome.presets {
            self.close_preset_menu(cx);
        }
        self.chrome = chrome;
        self.push_host_chrome();
        cx.notify();
    }

    /// The window's title: the channel it belongs to, then the plug-in in front.
    ///
    /// The channel leads because the window is the channel's — the tab strip
    /// already names every plug-in in it.
    fn window_title(&self) -> String {
        let track = self.chrome.track_name.as_str();
        let plugin = self.chrome.window_title();
        if track.is_empty() {
            return plugin;
        }
        format!("{track} - {plugin}")
    }

    /// Replaces the tab list. Cheap to call every poll — an unchanged list
    /// notifies nothing.
    pub(crate) fn set_tabs(&mut self, tabs: Vec<PluginEditorTab>, cx: &mut Context<Self>) {
        if self.tabs == tabs {
            return;
        }
        self.tabs = tabs;
        self.push_host_chrome();
        cx.notify();
    }

    /// Whether this window is hosting `insert_id`, on any of its tabs.
    pub(crate) fn hosts_insert(&self, insert_id: &str) -> bool {
        self.insert_id == insert_id || self.tabs.iter().any(|tab| tab.insert_id == insert_id)
    }

    /// The channel this window belongs to.
    pub(crate) fn track_id(&self) -> &str {
        &self.track_id
    }

    /// Brings another of this channel's plug-ins to the front.
    ///
    /// The current view is released first: the window has one region for a
    /// plug-in to live in, and two views in it at once is not something a
    /// plug-in has to tolerate. The lifecycle then restarts from `Opening` for
    /// the new insert, exactly as it did when the window was created.
    pub(crate) fn activate_tab(
        &mut self,
        insert_id: &str,
        display_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.insert_id == insert_id {
            return;
        }
        // A list of the previous plug-in's presets must not survive the switch.
        self.close_preset_menu(cx);
        self.release_active_view();
        self.insert_id = insert_id.to_string();
        self.display_name = display_name.to_string();
        self.chrome = PluginEditorChrome {
            plugin_name: display_name.to_string(),
            ..PluginEditorChrome::default()
        };
        self.status = PluginEditorStatus::Opening;
        self.wait_ticks = 0;
        self.host_mounted_logged = false;
        self.last_region = None;
        self.editor_content_size = None;
        self.host_preferred_size = None;
        self.host_auto_size_applied = false;
        self.host_auto_size_settled = false;
        if let Some(host) = self.host.as_mut() {
            host.last_region = None;
        }
        let _ = window;
        self.schedule_tick(cx);
        cx.notify();
    }

    /// Detaches whatever this window currently hosts, without closing it.
    fn release_active_view(&mut self) {
        if self.embed_handle.take().is_some() {
            if let Some(processor) = self.processor.as_ref() {
                processor.view_detach();
            }
        }
        if let Some(host) = self.host.as_mut() {
            let closing = self.insert_id.clone();
            if let Some(shared) = host.shared.as_ref() {
                if let Ok(mut runtime) = shared.lock() {
                    runtime.close_editor(closing.clone());
                }
            } else if let Some(client) = host.client.as_mut() {
                let _ = client.close_editor(format!("{}::{}", self.track_id, closing));
            }
            host.content = None;
        }
        self.view_content = None;
    }

    /// Whether a plug-in view is actually attached in this window.
    ///
    /// The studio asks before acting on a close from the host: a window that has
    /// not attached yet has no editor to lose, and the close belongs to whatever
    /// was torn down to make room for it.
    pub(crate) fn is_attached(&self) -> bool {
        matches!(self.status, PluginEditorStatus::Attached(_))
    }

    /// The insert slot this window is editing, as the studio addresses it.
    pub(crate) fn insert_key(&self) -> (&str, &str) {
        (self.track_id.as_str(), self.insert_id.as_str())
    }

    /// Drains the chrome controls the user pressed since the last call.
    pub(crate) fn take_chrome_actions(&mut self) -> Vec<PluginEditorAction> {
        std::mem::take(&mut self.chrome_actions)
    }

    /// Entry point for events routed by `StudioLayout` in shared-bridge mode.
    /// The shared runtime queue is drained in exactly one place (StudioLayout),
    /// which dispatches each editor-targeted event here so this window can leave
    /// "Loading" and apply the plug-in's preferred size. Mirrors the client-mode
    /// self-drain in `drive_host` (spec Part 2/4/5/6).
    pub(crate) fn ingest_host_event(
        &mut self,
        event: ClientEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.on_host_event(event, cx);
        // Preferred-size events arrive here; auto-size the shell as soon as one
        // lands (Part 3) rather than only on the next render-driven `drive_host`.
        self.apply_host_preferred_size(window);
    }

    /// The plugin instance id an editor-targeted host event refers to, if any.
    /// Used by `StudioLayout` to route to the owning editor window.
    pub(crate) fn editor_event_instance_id(event: &ClientEvent) -> Option<&str> {
        match event {
            ClientEvent::Host(HostEvent::EditorAttached {
                plugin_instance_id, ..
            })
            | ClientEvent::Host(HostEvent::EditorAttachFailed {
                plugin_instance_id, ..
            })
            | ClientEvent::Host(HostEvent::EditorClosed {
                plugin_instance_id, ..
            })
            | ClientEvent::Host(HostEvent::EditorPreferredSize {
                plugin_instance_id, ..
            })
            | ClientEvent::Host(HostEvent::EditorUnresponsive {
                plugin_instance_id, ..
            })
            | ClientEvent::Host(HostEvent::EditorChromeAction {
                plugin_instance_id, ..
            }) => Some(plugin_instance_id.as_str()),
            _ => None,
        }
    }

    /// Fold a host event into the existing `PluginEditorStatus` state machine.
    fn on_host_event(&mut self, ev: ClientEvent, cx: &mut Context<Self>) {
        let id = self.editor_id();
        match ev {
            ClientEvent::Host(HostEvent::Ready { pid, .. }) => {
                if let Some(host) = self.host.as_mut() {
                    host.host_pid = Some(pid);
                }
                eprintln!("[plugin-view][host] host ready pid={pid} editor_id={id}");
            }
            ClientEvent::Host(HostEvent::Pong { pid }) => {
                if let Some(host) = self.host.as_mut() {
                    host.host_pid = Some(pid);
                }
                eprintln!("[plugin-bridge] received Pong (late) pid={pid} editor_id={id}");
            }
            ClientEvent::Host(HostEvent::EditorAttached {
                result,
                preferred_width,
                preferred_height,
                ..
            }) => {
                eprintln!(
                    "[plugin-view][host] EditorAttached editor_id={id} attached_result={result} \
                     preferred={preferred_width}x{preferred_height}"
                );
                let backend = EditorBackendKind::current();
                if backend.embeds_in_editor_window() {
                    // Content is a WS_CHILD embed under the GPUI window, so the
                    // shell grows to make room for it.
                    if !self.host_auto_size_applied {
                        let size =
                            Self::preferred_size_or_default(preferred_width, preferred_height);
                        if Self::valid_preferred_size(preferred_width, preferred_height).is_none() {
                            eprintln!(
                                "[plugin-editor-window] preferred_size_invalid using_default={}x{}",
                                size.0, size.1
                            );
                        }
                        self.host_preferred_size = Some(size);
                    }
                } else {
                    // A host-owned window carries its own size. Recording it is
                    // still worth doing — a reopen asks for the size the plug-in
                    // last reported instead of the 900x600 fallback — but this
                    // shell must not grow to it.
                    if let Some(size) =
                        Self::valid_preferred_size(preferred_width, preferred_height)
                    {
                        self.editor_content_size = Some(size);
                    }
                }
                let was = self.status.clone();
                self.status = PluginEditorStatus::Attached(backend.presentation());
                if !backend.embeds_in_editor_window() {
                    // Anything pushed before now was dropped: the host had no
                    // editor window to draw a strip in. Forget what was sent so
                    // the next refresh is a full one rather than a no-op.
                    self.pushed_chrome = None;
                    self.push_host_chrome();
                }
                if !matches!(was, PluginEditorStatus::Attached(_)) {
                    eprintln!(
                        "[plugin-editor-window] plugin_instance_id={} editor_window_id={id}",
                        self.insert_id
                    );
                    eprintln!("[plugin-editor-window] state {was:?} -> Attached");
                    eprintln!(
                        "[plugin-editor-window] hide_loading_overlay instance={}",
                        self.insert_id
                    );
                }
                cx.notify();
            }
            ClientEvent::Host(HostEvent::EditorAttachFailed { error, .. }) => {
                eprintln!("[plugin-view][host] EditorAttachFailed editor_id={id} error={error}");
                self.status = PluginEditorStatus::Failed(error);
                cx.notify();
            }
            ClientEvent::Host(HostEvent::EditorClosed { .. }) => {
                eprintln!("[plugin-view][host] EditorClosed editor_id={id}");
            }
            ClientEvent::Host(HostEvent::EditorChromeAction { action, .. }) => {
                // Straight onto the queue this window's own strip would have
                // used. The studio drains it and applies it through one
                // function, so a press on the AppKit strip and a press on the
                // GPUI one cannot come to mean different things.
                use SpherePluginHost::ipc::EditorChromeCommand;
                let action = match action {
                    EditorChromeCommand::SetActive { active } => {
                        PluginEditorAction::SetActive(active)
                    }
                    EditorChromeCommand::StepPreset { delta } => {
                        PluginEditorAction::StepPreset(delta)
                    }
                    EditorChromeCommand::SavePreset => PluginEditorAction::SavePreset,
                    EditorChromeCommand::SelectPreset { index } => {
                        PluginEditorAction::SelectPreset(index as usize)
                    }
                    EditorChromeCommand::SelectTab { insert_id } => {
                        PluginEditorAction::SelectTab(insert_id)
                    }
                    EditorChromeCommand::CloseTab { insert_id } => {
                        PluginEditorAction::CloseTab(insert_id)
                    }
                };
                eprintln!("[plugin-editor-chrome] host strip action={action:?} editor_id={id}");
                self.chrome_actions.push(action);
                cx.notify();
            }
            ClientEvent::Host(HostEvent::EditorUnresponsive { gap_ms, .. }) => {
                // Freeze-watchdog notification; the host usually recovers, so
                // log only — the close path stays available in this process.
                eprintln!("[plugin-view][host] EditorUnresponsive editor_id={id} gap_ms={gap_ms}");
            }
            ClientEvent::Host(HostEvent::EditorContentResize {
                plugin_instance_id,
                width,
                height,
                dpi: _,
            }) => {
                eprintln!(
                    "[plugin-bridge] event EditorContentResize instance={plugin_instance_id} width={width} height={height}"
                );
                let size = Self::preferred_size_or_default(width, height);
                self.host_preferred_size = Some(size);
                self.editor_content_size = Some(size);
                cx.notify();
            }
            ClientEvent::Host(HostEvent::EditorPreferredSize {
                plugin_instance_id,
                width,
                height,
            }) => {
                eprintln!(
                    "[plugin-bridge] event EditorPreferredSize instance={plugin_instance_id} width={width} height={height}"
                );
                let size = Self::preferred_size_or_default(width, height);
                if Self::valid_preferred_size(width, height).is_none() {
                    eprintln!(
                        "[plugin-editor-window] preferred_size_invalid using_default={}x{}",
                        size.0, size.1
                    );
                }
                self.host_preferred_size = Some(size);
                if !self.host_auto_size_applied {
                    self.editor_content_size = Some(size);
                }
                cx.notify();
            }
            ClientEvent::Host(HostEvent::PluginLoading { .. })
            | ClientEvent::Host(HostEvent::PluginLoaded { .. })
            | ClientEvent::Host(HostEvent::PluginAlreadyLoaded { .. })
            | ClientEvent::Host(HostEvent::PluginLoadFailed { .. }) => {}
            ClientEvent::Host(HostEvent::PluginUnloaded { .. }) => {}
            // Audio-bridge events are handled by StudioLayout, not the editor window.
            ClientEvent::Host(HostEvent::AudioBridgeConfigured { .. })
            | ClientEvent::Host(HostEvent::AudioBridgeStatus { .. })
            | ClientEvent::Host(HostEvent::SharedAudioAttached { .. })
            | ClientEvent::Host(HostEvent::ProcessingPrepared { .. }) => {}
            // Plugin-state replies are consumed by the save/restore flow in
            // PluginBridgeRuntime; nothing to fold into editor status.
            ClientEvent::Host(HostEvent::PluginState { .. })
            | ClientEvent::Host(HostEvent::PluginStateSet { .. })
            | ClientEvent::Host(HostEvent::PluginParameters { .. })
            // Built-in NAM results are routed to the built-in editor windows
            // by `poll_plugin_bridge_runtime`, not this VST3 state machine.
            | ClientEvent::Host(HostEvent::BuiltinNamCaptureResult { .. })
            | ClientEvent::Host(HostEvent::BuiltinIrResult { .. }) => {}
            // Transport keys are a workspace command, not editor state — the
            // studio layout runs them (see `poll_plugin_bridge_runtime`).
            ClientEvent::Host(HostEvent::TransportToggleRequested { .. }) => {}
            ClientEvent::Host(HostEvent::Log { level, message }) => {
                eprintln!("[plugin-view][host][{level}] {message}");
            }
            ClientEvent::Disconnected => {
                eprintln!(
                    "[plugin-view][host] EditorDisconnected editor_id={id} (host process exited/crashed)"
                );
                self.status = PluginEditorStatus::Failed(
                    "Plugin host process disconnected (crashed or exited). \
                     The editor closed; audio is unaffected."
                        .to_string(),
                );
                cx.notify();
            }
        }
    }

    /// Push a resized content rect to both the content child HWND (geometry,
    /// owned by the main app) and the host (`ResizeEditor` → `onSize`).
    fn sync_host_region(&mut self, window: &mut Window) {
        self.maybe_release_initial_preferred_size(window);
        let region = self.host_region_for(window);
        let rect = ContentRect {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
        };
        let dpi = (window.scale_factor().max(0.5) * 96.0).round() as u32;
        let id = self.editor_id();
        let Some(host) = self.host.as_mut() else {
            return;
        };
        if host.last_region == Some(rect) {
            return;
        }
        let size_changed = host
            .last_region
            .map(|previous| previous.width != rect.width || previous.height != rect.height)
            .unwrap_or(true);
        host.last_region = Some(rect);
        if let Some(content) = host.content.as_ref() {
            if !content.is_valid() {
                return;
            }
            content.set_bounds(rect);
        }
        if size_changed {
            eprintln!(
                "[plugin-bridge] sending ResizeEditor instance={} width={} height={} dpi={dpi}",
                self.insert_id, rect.width, rect.height
            );
            if let Some(shared) = host.shared.as_ref() {
                if let Ok(mut runtime) = shared.lock() {
                    runtime.resize_editor(
                        self.insert_id.clone(),
                        rect.width as u32,
                        rect.height as u32,
                        dpi,
                    );
                }
            } else if let Some(client) = host.client.as_mut() {
                let _ =
                    client.resize_editor(id.clone(), rect.width as u32, rect.height as u32, dpi);
            }
        }
        if plugin_view_debug() {
            eprintln!(
                "[plugin-view][host] resize editor_id={id} content=({},{},{}x{})",
                rect.x, rect.y, rect.width, rect.height
            );
        }
    }

    fn viewport_content_size(&self, window: &Window) -> (i32, i32) {
        let scale = window.scale_factor().max(0.5);
        let viewport = window.viewport_size();
        let w: f32 = viewport.width.into();
        let h: f32 = viewport.height.into();
        let header_px = HEADER_H * scale;
        (
            (w * scale).round().max(1.0) as i32,
            ((h * scale) - header_px).round().max(1.0) as i32,
        )
    }

    fn maybe_release_initial_preferred_size(&mut self, window: &Window) {
        if !self.bridge_required || !self.host_auto_size_applied {
            return;
        }
        let Some((preferred_w, preferred_h)) = self.editor_content_size else {
            return;
        };
        let (viewport_w, viewport_h) = self.viewport_content_size(window);
        let close_to_preferred =
            (viewport_w - preferred_w).abs() <= 2 && (viewport_h - preferred_h).abs() <= 2;
        if !self.host_auto_size_settled {
            if close_to_preferred {
                self.host_auto_size_settled = true;
            }
            return;
        }
        if !close_to_preferred {
            self.editor_content_size = None;
        }
    }

    fn apply_host_preferred_size(&mut self, window: &mut Window) {
        if self.host_auto_size_applied {
            return;
        }
        let Some((content_w, content_h)) = self.host_preferred_size else {
            return;
        };
        self.editor_content_size = Some((content_w, content_h));
        let scale = window.scale_factor().max(0.5);
        let shell_w = (content_w as f32 / scale).max(EDITOR_WINDOW_MIN_WIDTH);
        let shell_h = ((content_h as f32 / scale) + HEADER_H).max(EDITOR_WINDOW_MIN_HEIGHT);
        let viewport = window.viewport_size();
        let current_w: f32 = viewport.width.into();
        let current_h: f32 = viewport.height.into();
        eprintln!(
            "[editor-size] plugin preferred size = {}x{}",
            content_w, content_h
        );
        eprintln!("[editor-size] titlebar height = {:.0}", HEADER_H * scale);
        eprintln!("[editor-size] client rect = {}x{}", content_w, content_h);
        eprintln!(
            "[editor-size] shell outer target = {:.0}x{:.0}",
            shell_w, shell_h
        );
        if (current_w - shell_w).abs() > 1.0 || (current_h - shell_h).abs() > 1.0 {
            eprintln!(
                "[plugin-editor-window] auto_size content={}x{} shell={:.0}x{:.0}",
                content_w, content_h, shell_w, shell_h
            );
            window.resize(size(px(shell_w), px(shell_h)));
        }
        let region = self.host_region_for(window);
        eprintln!(
            "[editor-size] attach rect = {}/{}/{}/{}",
            region.x,
            region.y,
            region.x + region.width,
            region.y + region.height
        );
        self.sync_host_region(window);
        self.host_auto_size_applied = true;
        self.host_auto_size_settled = false;
    }
}

impl Drop for PluginEditorWindow {
    /// The preset list is deliberately *not* closed here: `drop` has no `App`,
    /// so no `WindowHandle` can be updated from it. On Windows the list is an
    /// owned window and the OS destroys it with this one; every other path that
    /// ends an editor — `activate_tab`, the titlebar close, the studio's
    /// `close_plugin_editor_tab` — closes it explicitly before the window goes.
    fn drop(&mut self) {
        if crate::shutdown::ShutdownState::global().is_shutting_down() {
            return;
        }
        // Host-process path: ask the host to remove the view (spec Part 6), then
        // let the backend's Drop tear down the content HWND + the host process.
        if let Some(host) = self.host.as_mut() {
            let id = format!("{}::{}", self.track_id, self.insert_id);
            if let Some(shared) = host.shared.as_ref() {
                if let Ok(mut runtime) = shared.lock() {
                    runtime.close_editor(self.insert_id.clone());
                }
            } else if let Some(client) = host.client.as_mut() {
                let _ = client.close_editor(id.clone());
            }
            eprintln!("[plugin-view][host] CloseEditor sent editor_id={id} (drop) — tearing down content HWND + host process");
            return;
        }
        if self.embed_handle.take().is_some() {
            // Detach the editor view + destroy the host window. The runtime
            // processor (and audio) keep running — only insert removal destroys it.
            if let Some(processor) = self.processor.as_ref() {
                processor.view_detach();
            }
            // The child window goes after the view has let go of it, which the
            // field order in this struct does not guarantee on its own.
            self.view_content = None;
            if plugin_view_debug() {
                eprintln!(
                    "[plugin-view] close editor_id={} (drop → detach view only, processor kept)",
                    self.editor_id()
                );
            }
        }
    }
}

impl PluginEditorWindow {
    fn render_status_message(&self, headline: &str) -> gpui::AnyElement {
        self.render_status_panel(headline, None)
    }

    /// The plug-in name, a headline, and optionally the sentence that explains
    /// it — the one surface every non-attached state of this window renders.
    fn render_status_panel(&self, headline: &str, detail: Option<&str>) -> gpui::AnyElement {
        let mut panel = div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .items_center()
            .justify_center()
            .size_full()
            .bg(Colors::surface_base())
            .p(px(20.0))
            .child(
                div()
                    .text_size(px(crate::theme::typography::UI_MD))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.display_name.clone()),
            )
            .child(
                div()
                    .text_size(px(crate::theme::typography::UI_SM))
                    .text_color(Colors::text_secondary())
                    .child(headline.to_string()),
            );
        if let Some(detail) = detail {
            panel = panel.child(
                div()
                    .text_size(px(crate::theme::typography::UI_XS))
                    .text_color(Colors::text_muted())
                    .child(detail.to_string()),
            );
        }
        panel.into_any_element()
    }

    /// A failure panel with no Retry.
    ///
    /// The distinction is the whole point of [`PluginEditorStatus::Unsupported`]:
    /// this window is not waiting for anything and no amount of trying will
    /// change it, so offering a button that re-runs the same refusal would be a
    /// control that lies about what it does.
    fn render_unsupported_panel(&self, reason: &str) -> gpui::AnyElement {
        let close = div()
            .id("plugin-editor-close")
            .px(px(14.0))
            .py(px(6.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .cursor(gpui::CursorStyle::PointingHand)
            .bg(Colors::surface_raised())
            .text_size(px(crate::theme::typography::UI_SM))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_secondary())
            .hover(|s| s.bg(Colors::surface_control_hover()))
            .child("Close")
            .on_click(|_ev, window, _cx| window.remove_window());

        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .items_center()
            .justify_center()
            .size_full()
            .bg(Colors::surface_base())
            .p(px(20.0))
            .child(
                div()
                    .text_size(px(crate::theme::typography::UI_MD))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.display_name.clone()),
            )
            .child(
                div()
                    .text_size(px(crate::theme::typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_secondary())
                    .child("Editor not available on this platform"),
            )
            .child(
                div()
                    .max_w(px(460.0))
                    .text_size(px(crate::theme::typography::UI_SM))
                    .text_color(Colors::text_secondary())
                    .child(reason.to_string()),
            )
            .child(close)
            .into_any_element()
    }

    fn render_failure_panel(&self, err: &str, cx: &mut Context<Self>) -> gpui::AnyElement {
        let retry = div()
            .id("plugin-editor-retry")
            .px(px(14.0))
            .py(px(6.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .cursor(gpui::CursorStyle::PointingHand)
            .bg(Colors::accent_muted())
            .text_size(px(crate::theme::typography::UI_SM))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::accent_primary())
            .hover(|s| s.bg(Colors::surface_control_hover()))
            .child("Retry")
            .on_click(cx.listener(|this, _ev, _window, cx| this.retry(cx)));

        let close = div()
            .id("plugin-editor-close")
            .px(px(14.0))
            .py(px(6.0))
            .rounded(px(crate::theme::radius::CONTROL))
            .cursor(gpui::CursorStyle::PointingHand)
            .bg(Colors::surface_raised())
            .text_size(px(crate::theme::typography::UI_SM))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_secondary())
            .hover(|s| s.bg(Colors::surface_control_hover()))
            .child("Close")
            .on_click(|_ev, window, _cx| window.remove_window());

        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .items_center()
            .justify_center()
            .size_full()
            .bg(Colors::surface_base())
            .p(px(20.0))
            .child(
                div()
                    .text_size(px(crate::theme::typography::UI_MD))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.display_name.clone()),
            )
            .child(
                div()
                    .text_size(px(crate::theme::typography::UI_SM))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::status_error())
                    .child("Editor failed to open"),
            )
            .child(
                div()
                    .max_w(px(560.0))
                    .text_size(px(crate::theme::typography::UI_SM))
                    .text_color(Colors::text_secondary())
                    .child(err.to_string()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(retry)
                    .child(close),
            )
            .into_any_element()
    }
}

impl Render for PluginEditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The lifecycle does not run here. Creating the content child window,
        // sending OpenEditor and resizing this window all dispatch Win32
        // messages that re-enter GPUI, and re-entering during a draw panics in
        // the element arena. `render` records the window it is drawing into and
        // hands the work to the deferred tick — the same rule the docked ARA
        // panel follows for the same reason.
        //
        // Where this window sits on screen is recorded here too: the preset list
        // is a window of its own, so it has to be placed in screen coordinates,
        // clamped to this window's client rect, owned by this window's HWND and
        // scaled by this window's DPI — and the click that opens it arrives with
        // no `Window` to ask.
        self.window_bounds = window.bounds();
        self.window_viewport = window.viewport_size();
        self.window_scale = window.scale_factor();
        self.window_hwnd = Self::native_parent_handle(window);
        if self.preset_menu_refocus {
            self.preset_menu_refocus = false;
            window.focus(&self.focus_handle, cx);
        }
        self.schedule_tick(cx);

        // When attached, GPUI must not paint anything below the header. The
        // plug-in's surface there is a native `WS_CHILD` window, and the app
        // boots with `GPUI_DISABLE_DIRECT_COMPOSITION=1` precisely so that child
        // composites *above* this window's swap chain
        // (`apps/native/studio/src/main.rs:110`). gpui answers by creating this
        // window with `WS_CLIPCHILDREN`, which removes the child's rectangle
        // from this window's visible region — so anything drawn there is not
        // "hidden behind the plug-in", it is never composited at all. Only draw
        // overlays while opening / waiting / attaching / failed.
        let content_overlay: Option<gpui::AnyElement> = match &self.status {
            PluginEditorStatus::Opening if self.bridge_required => {
                Some(self.render_status_message(&format!("Loading: {}", self.display_name)))
            }
            PluginEditorStatus::Opening => Some(self.render_status_message("Opening editor…")),
            PluginEditorStatus::WaitingForHostHandle => {
                if self.bridge_required {
                    Some(self.render_status_message(&format!("Loading: {}", self.display_name)))
                } else {
                    Some(self.render_status_message("Opening editor… (waiting for host window)"))
                }
            }
            PluginEditorStatus::Attaching => {
                if self.bridge_required {
                    Some(self.render_status_message(&format!("Loading: {}", self.display_name)))
                } else {
                    Some(self.render_status_message("Attaching plugin editor…"))
                }
            }
            PluginEditorStatus::ProbingReady { probe_index, .. } => {
                let step = (*probe_index as usize).saturating_add(1);
                let total = READY_PROBE_DELAYS_MS.len();
                Some(self.render_status_message(&format!(
                    "Opening editor… (waiting for plug-in UI, {step}/{total})"
                )))
            }
            PluginEditorStatus::Failed(err) => {
                let err = err.clone();
                Some(self.render_failure_panel(&err, cx))
            }
            PluginEditorStatus::Unsupported(reason) => {
                let reason = reason.clone();
                Some(self.render_unsupported_panel(&reason))
            }
            PluginEditorStatus::Attached(PluginEditorPresentationMode::DetachedNativeWindow) => {
                // The plug-in is in a window the host process owns, so this
                // shell has no plug-in region to expose. It keeps the controls
                // that belong to the plug-in but cannot be drawn over its
                // surface, and says where the surface went — an empty panel
                // here reads as an editor that failed to open.
                Some(self.render_status_panel(
                    "Its editor is open in a window of its own.",
                    Some(
                        "The controls above still apply to it. Closing this window \
                         closes the editor.",
                    ),
                ))
            }
            PluginEditorStatus::Attached(mode) => {
                // Nothing is drawn here in normal operation: the single active
                // host HWND owns this region and `WS_CLIPCHILDREN` has removed
                // it from this window's visible region, so an element here is
                // built every frame and composited never.
                //
                // The debug overlay below is the experiment that says so out
                // loud: with `FUTUREBOARD_PLUGIN_VIEW_DEBUG=1` an opaque plate
                // is drawn over the whole content region, and if the plug-in's
                // UI is still what you see, GPUI cannot paint over the child —
                // which is why the preset list has to be a window of its own.
                if plugin_view_debug() {
                    Some(
                        div()
                            .absolute()
                            .top(px(HEADER_H))
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(Colors::surface_base())
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(Colors::text_secondary())
                                    .child(format!("External editor overlay active ({mode:?})")),
                            )
                            .into_any_element(),
                    )
                } else {
                    None
                }
            }
        };

        let mut root = div()
            .relative()
            .size_full()
            .font(theme::ui_font())
            .overflow_hidden()
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(crate::components::title_bar::external_window_titlebar(
                self.window_title(),
                "plugin-editor-window-close",
                {
                    // The list is a window of its own; closing the editor with
                    // it open must not leave it behind on platforms where the
                    // OS owner relationship does not do it for us.
                    let this = cx.entity().downgrade();
                    move |window, cx| {
                        let _ = this.update(cx, |editor, cx| editor.close_preset_menu(cx));
                        window.remove_window();
                    }
                },
            ))
            .child(render_tab_strip(&self.tabs, &self.insert_id, {
                let this = cx.entity().downgrade();
                move |action, cx| {
                    let _ = this.update(cx, |editor, cx| {
                        editor.chrome_actions.push(action);
                        cx.notify();
                    });
                }
            }))
            .child(render_chrome_tools(
                &self.chrome,
                self.preset_menu.is_some() || self.preset_menu_requested,
                Rc::clone(&self.preset_anchor),
                {
                    let this = cx.entity().downgrade();
                    move |action, cx| {
                        // Queued on the window; the studio drains it on its next
                        // poll. Applying it here would need the insert and the
                        // engine, neither of which this window owns. Opening the
                        // preset list is the exception — it is this window's own
                        // state and nothing else has to know.
                        let _ = this.update(cx, |editor, cx| {
                            match action {
                                PluginEditorAction::TogglePresetMenu(true) => {
                                    editor.request_preset_menu(cx);
                                }
                                PluginEditorAction::TogglePresetMenu(false) => {
                                    editor.close_preset_menu(cx);
                                }
                                action => {
                                    editor.close_preset_menu(cx);
                                    editor.chrome_actions.push(action);
                                }
                            }
                            cx.notify();
                        });
                    }
                },
            ))
            // The plug-in's own surface sits below the header. It is a native
            // child window whose rectangle `WS_CLIPCHILDREN` has already taken
            // out of this window's visible region, so a ground painted here
            // reaches only the part of that region the child does not cover —
            // it stops the swap chain's last contents showing through wherever
            // the plug-in does not reach, and it can neither hide the plug-in
            // nor be drawn on top of it.
            .child(
                div()
                    .absolute()
                    .top(px(HEADER_H))
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .bg(Colors::surface_base()),
            );

        if let Some(overlay) = content_overlay {
            root = root.child(
                div()
                    .absolute()
                    .top(px(HEADER_H))
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .child(overlay),
            );
        }

        root
    }
}

/// Open the GPUI-hosted plugin editor window for an insert slot. The caller
/// (StudioLayout) keeps the returned handle to dedupe/close. Drop of the entity
/// detaches the native view.
#[allow(clippy::too_many_arguments)]
/// Opens an editor window for one plug-in instance.
///
/// `in_process` forces the in-process embed path regardless of the bridge
/// setting. It exists for ARA, whose instances are always hosted here; every
/// other caller passes `false` and gets the mandatory external bridge.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_plugin_editor_window(
    owner_bounds: Bounds<gpui::Pixels>,
    track_id: String,
    insert_id: String,
    display_name: String,
    processor: Option<DirectAudio::Vst3RuntimeProcessor>,
    shared_bridge: Option<SharedPluginBridgeRuntime>,
    in_process: bool,
    cx: &mut App,
) -> Result<WindowHandle<PluginEditorWindow>, String> {
    if in_process {
        eprintln!(
            "[plugin-view] editor_backend=in_process reason=ara instance={track_id}::{insert_id}"
        );
    } else if plugin_host_bridge_enabled() {
        eprintln!(
            "[plugin-view] editor_backend=external_bridge reason=forced_default \
             instance={track_id}::{insert_id}"
        );
    } else {
        eprintln!(
            "[plugin-view] editor_backend=in_process reason=FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS=1 instance={track_id}::{insert_id}"
        );
        eprintln!("[plugin-runtime] WARNING using legacy in-process plugin runtime");
        eprintln!("[plugin-runtime] legacy path may hang GPU/browser-backed plugin editors");
    }
    if plugin_view_debug() {
        eprintln!(
            "[plugin-view] open requested plugin={display_name} track={track_id} insert={insert_id} instance={}::{}",
            track_id, insert_id
        );
    }
    let parent_x: f32 = owner_bounds.origin.x.into();
    let parent_y: f32 = owner_bounds.origin.y.into();
    let parent_w: f32 = owner_bounds.size.width.into();
    let parent_h: f32 = owner_bounds.size.height.into();
    let origin = Point {
        x: px(parent_x + ((parent_w - EDITOR_WINDOW_WIDTH) / 2.0).max(24.0)),
        y: px(parent_y + ((parent_h - EDITOR_WINDOW_HEIGHT) / 2.0).max(24.0)),
    };

    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(Bounds {
        origin,
        size: size(px(EDITOR_WINDOW_WIDTH), px(EDITOR_WINDOW_HEIGHT)),
    }));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = false;
    // Opaque shell: Transparent uses ACCENT_ENABLE_TRANSPARENTGRADIENT and shows
    // whatever window is *behind* this floating editor (timeline bleed-through).
    // The VST3 UI is a WS_CHILD under this HWND; with DirectComposition disabled
    // at app boot it composites above the swap chain in the content region.
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(EDITOR_WINDOW_MIN_WIDTH),
        px(EDITOR_WINDOW_MIN_HEIGHT),
    ));
    // Without this the requested rect is validated against the PRIMARY monitor
    // (`gpui_windows/src/window.rs:1713`) and replaced by a default centred on
    // it whenever the studio is on any other one — the editor lands on the
    // wrong screen, and the preset list that anchors to it follows. Every other
    // external window in this crate already does this.
    crate::window_position::apply_owner_display(&mut options, Some(owner_bounds), cx);
    // On a host-owned backend this window has nothing to show: the plug-in and
    // the chrome above it are both in the host process's own window. The entity
    // still exists and still does its job — it drives the attach, carries the
    // tabs, pushes the chrome and closes the editor when it goes — but it is
    // never ordered on screen, because a second empty frame beside the editor
    // is exactly what porting the strip to AppKit was for.
    options.show = EditorBackendKind::current().embeds_in_editor_window();
    options.focus = options.show;

    let editor_id = format!("{track_id}::{insert_id}");
    let result = cx.open_window(options, |_window, cx| {
        cx.new(|cx| {
            PluginEditorWindow::new(
                track_id,
                insert_id,
                display_name,
                processor,
                shared_bridge,
                in_process,
                cx,
            )
        })
    });
    if plugin_view_debug() {
        match &result {
            Ok(_) => eprintln!("[plugin-view] gpui window created id={editor_id}"),
            Err(e) => eprintln!("[plugin-view] gpui window create FAILED id={editor_id} err={e}"),
        }
    }
    // The attach lifecycle is normally started by the first `render`, which is
    // where `schedule_tick` is called from. A host-owned shell is never shown
    // and must not be assumed to draw, so it is started here instead —
    // otherwise the editor would depend on a frame that may never come. The
    // tick is idempotent, so this is a no-op if a frame does arrive first.
    if let Ok(handle) = result.as_ref() {
        if !EditorBackendKind::current().embeds_in_editor_window() {
            let _ = handle.update(cx, |editor, _window, cx| editor.schedule_tick(cx));
        }
    }
    result.map_err(|e| e.to_string())
}

#[cfg(test)]
mod platform_support_tests {
    use super::*;

    /// Every refusal must land on `Unsupported`, which is the variant whose
    /// panel has no Retry. `Failed` keeps Retry, and retrying a platform that
    /// has no implementation runs the same refusal again — the loop the macOS
    /// dialog put people in.
    #[test]
    fn a_refused_editor_is_unsupported_not_failed() {
        for reason in [
            LocalEditorCompatibility::BackendNotImplemented,
            LocalEditorCompatibility::UnsupportedSplitController,
            LocalEditorCompatibility::ControllerCreationFailed,
            LocalEditorCompatibility::ViewUnsupported,
            LocalEditorCompatibility::ViewAttachFailed,
        ] {
            let status = unsupported_status(&reason);
            match status {
                PluginEditorStatus::Unsupported(message) => {
                    assert_eq!(message, reason.message());
                    assert!(!message.is_empty());
                }
                other => panic!("{reason:?} produced {other:?}, which offers Retry"),
            }
        }
    }

    /// The window must not give up on the backend that works. A macOS change
    /// that made Windows stop waiting for its parent handle would break the
    /// platform that ships today.
    #[test]
    fn the_remote_backend_still_waits_for_its_parent_handle() {
        assert_eq!(
            host_region_wait(
                EditorBackendKind::RemoteNativeWindow,
                local_native_view_ready()
            ),
            HostRegionWait::KeepWaiting,
            "Windows must keep waiting regardless of the local-view build-out"
        );
        assert!(EditorBackendKind::RemoteNativeWindow.waits_for_foreign_parent_handle());
    }

    /// What this build actually does when the host region is not ready.
    #[test]
    fn this_build_resolves_to_its_own_backend() {
        let backend = EditorBackendKind::current();
        let wait = host_region_wait(backend, local_native_view_ready());
        if cfg!(target_os = "windows") || cfg!(target_os = "macos") || cfg!(target_os = "linux") {
            assert_eq!(
                wait,
                HostRegionWait::KeepWaiting,
                "every platform that ships has a backend that can open an editor"
            );
        } else {
            assert_eq!(
                wait,
                HostRegionWait::GiveUp(LocalEditorCompatibility::BackendNotImplemented),
                "a platform without a finished backend reports it instead of \
                 spinning out a wait it cannot win"
            );
        }
    }

    /// The shell only reserves a plug-in region on the backends that put a
    /// plug-in surface in it.
    ///
    /// A host-owned editor presented as a child embed is the failure this
    /// guards: the shell would grow to a size nothing paints into, and
    /// `sync_host_region` would push this window's geometry at a window in
    /// another process on every layout pass.
    #[test]
    fn only_an_embedded_backend_owns_a_plugin_region() {
        let backend = EditorBackendKind::current();
        assert_eq!(
            backend.embeds_in_editor_window(),
            cfg!(target_os = "windows"),
            "Windows is the only platform that can reparent a plug-in's view \
             into this window"
        );
        assert_eq!(
            backend.presentation() == PluginEditorPresentationMode::DetachedNativeWindow,
            !backend.embeds_in_editor_window()
        );
    }

    /// The compact shell has to leave room for the chrome it exists to carry.
    #[test]
    fn the_host_owned_shell_is_at_least_its_own_chrome() {
        let height = HEADER_H + HOST_OWNED_SHELL_BODY_H;
        assert!(height > HEADER_H);
        assert!(height >= EDITOR_WINDOW_MIN_HEIGHT);
        assert!(HOST_OWNED_SHELL_WIDTH >= EDITOR_WINDOW_MIN_WIDTH);
    }

    /// The size a host-owned editor is opened at is the plug-in's own logical
    /// size, so the DPI sent beside it must be the unscaled one. Sending a
    /// backing-scaled DPI with a point size describes neither.
    #[test]
    fn a_host_owned_editor_is_requested_in_logical_units() {
        assert_eq!(HOST_OWNED_EDITOR_DPI, 96);
        let (w, h) = DEFAULT_PLUGIN_EDITOR_CONTENT_SIZE;
        assert!(w >= MIN_PLUGIN_EDITOR_CONTENT_SIZE && h >= MIN_PLUGIN_EDITOR_CONTENT_SIZE);
    }
}
