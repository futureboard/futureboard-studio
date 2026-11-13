//! The Rigs tab of the browser sidebar: rigs.futureboard.studio, embedded.
//!
//! # How it works
//!
//! The page is a real browser — the same CEF runtime the plug-in editors use —
//! in a native child window parked over the sidebar body, exactly the way
//! `AraEditorHost` parks a plug-in view over the editor dock:
//!
//! ```txt
//! studio window
//!   └── ContentChildHwnd (Windows) / DockedPluginSurface (macOS)
//!         └── CEF browser: https://rigs.futureboard.studio/?embed=daw
//! ```
//!
//! The browser is locked to that one origin. The page talks to Studio by
//! POSTing to `/__daw/bridge` on its own origin, which CEF answers here instead
//! of the network; Studio answers back with `window.postMessage`.
//!
//! # Account
//!
//! Studio is signed in to the same Futureboard account the site uses, so when
//! the page says it is ready, Studio hands it that session. The page never asks
//! the person to sign in again, and signing in or out happens in Studio.
//!
//! # Dragging onto the arrangement
//!
//! GPUI never sees the mouse inside a native child window, so a drag out of the
//! page cannot be a GPUI drag. The page reports the drag instead; Studio
//! downloads the file (already in flight since the button went down) and starts
//! an operating-system file drag with it while the button is still held. The
//! arrangement receives that like any file from Explorer: audio becomes a clip,
//! a `.pst` becomes a track or an insert. Where the platform cannot start a
//! file drag, the file is added at the playhead on a new track instead.
//!
//! # Lifetime
//!
//! The view exists only while the Rigs tab is on screen. `StudioLayout` asks
//! it to detach when the tab or the sidebar goes away; the browser is closed,
//! and its native parent is destroyed only after CEF has let go of it.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    div, px, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement, Pixels,
    Render, StatefulInteractiveElement, Styled, WeakEntity, Window,
};

use crate::components::builtin_plugin_editor::{
    self as host, RemoteView, ViewEvent, ViewId, ViewRect,
};
use crate::components::os_file_drag::{self, DragOutcome};
use crate::components::plugin_content_host::ContentRect;
use crate::components::transport_key::{self, TransportKeySource};
use crate::layout::StudioLayout;
use crate::theme::{space, typography, Colors};

/// The production site. Only this origin is ever loaded.
pub const RIGS_ORIGIN: &str = "https://rigs.futureboard.studio";

/// Inbound-queue tag for the page's bridge messages.
const TAG: &str = "rigs";

/// Smallest region worth a browser; below this the sidebar is collapsing.
const MIN_EMBED_PX: i32 = 24;

/// How long a drag waits for its file before giving up.
const DRAG_WAIT: Duration = Duration::from_secs(8);

/// Largest file Rigs serves (a 95 MB pack, or a 64 MB preset), with room.
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

#[cfg(target_os = "macos")]
type Surface = crate::components::plugin_editor_mac_region::DockedPluginSurface;
#[cfg(not(target_os = "macos"))]
type Surface = crate::components::plugin_content_host::ContentChildHwnd;

#[cfg(target_os = "macos")]
fn surface_create(parent: u64, rect: ContentRect) -> Option<Surface> {
    Surface::create(parent, region_px(rect))
}
#[cfg(not(target_os = "macos"))]
fn surface_create(parent: u64, rect: ContentRect) -> Option<Surface> {
    Surface::create(parent, rect)
}

#[cfg(target_os = "macos")]
fn surface_handle(surface: &Surface) -> u64 {
    surface.handle()
}
#[cfg(not(target_os = "macos"))]
fn surface_handle(surface: &Surface) -> u64 {
    surface.hwnd()
}

#[cfg(target_os = "macos")]
fn surface_set_bounds(surface: &Surface, parent: u64, rect: ContentRect) {
    surface.set_bounds(parent, region_px(rect));
}
#[cfg(not(target_os = "macos"))]
fn surface_set_bounds(surface: &Surface, _parent: u64, rect: ContentRect) {
    surface.set_bounds(rect);
}

#[cfg(target_os = "macos")]
fn region_px(rect: ContentRect) -> crate::components::plugin_editor_mac_region::RegionPx {
    crate::components::plugin_editor_mac_region::RegionPx {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// The page to load and the origin it is locked to.
///
/// A debug build may point at a local dev server with `FUTUREBOARD_RIGS_URL`
/// (for example `http://localhost:4322`); a release build always loads the
/// production site, so a stray environment variable cannot aim a shipped app
/// at some other origin.
fn remote_view() -> RemoteView {
    let origin = if cfg!(debug_assertions) {
        std::env::var("FUTUREBOARD_RIGS_URL")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| value.starts_with("https://") || value.starts_with("http://localhost"))
            .unwrap_or_else(|| RIGS_ORIGIN.to_string())
    } else {
        RIGS_ORIGIN.to_string()
    };
    RemoteView {
        tag: TAG,
        url: format!("{origin}/?embed=daw"),
        origin,
    }
}

/// Whether this build and platform can host the page in the sidebar.
fn embedding_supported() -> bool {
    cfg!(feature = "builtin-plugin-editor") && !host::OFFSCREEN_HOSTING
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    /// Waiting for a measured rect, or for the browser to be created.
    Opening,
    /// The page is parked over the sidebar body.
    Attached,
    /// Nothing is shown, and this is why.
    Failed(String),
}

/// A file the page asked for: what is known about it so far.
#[derive(Clone, Debug)]
enum Download {
    Pending,
    Ready(PathBuf),
    Failed(String),
}

/// One thing the page can be dragged out of: a preset, a single-sample share,
/// or one sample of a pack.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FileRef {
    slug: String,
    file_id: Option<String>,
}

impl FileRef {
    /// The download URL on `origin`, built from validated parts only.
    fn url(&self, origin: &str) -> Option<reqwest::Url> {
        let mut url = reqwest::Url::parse(origin).ok()?;
        {
            let mut segments = url.path_segments_mut().ok()?;
            segments.push("rigs").push(&self.slug);
            if let Some(id) = &self.file_id {
                segments.push("files").push(id);
            }
            segments.push("download");
        }
        Some(url)
    }
}

/// A bridge message from the page.
#[derive(Debug, PartialEq, Eq)]
enum Inbound {
    Ready,
    SignIn,
    Prepare(FileRef),
    Drag(FileRef),
}

fn parse_inbound(body: &[u8]) -> Option<Inbound> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let kind = value.get("type")?.as_str()?;
    let file = || -> Option<FileRef> {
        let slug = value.get("slug")?.as_str()?.trim();
        // A slug is a path segment the site minted; anything long or empty is
        // not one.
        if slug.is_empty() || slug.chars().count() > 120 {
            return None;
        }
        let file_id = match value.get("fileId").and_then(|v| v.as_str()) {
            Some(id) if is_uuid(id) => Some(id.to_ascii_lowercase()),
            Some(_) => return None,
            None => None,
        };
        Some(FileRef {
            slug: slug.to_string(),
            file_id,
        })
    };
    match kind {
        "ready" => Some(Inbound::Ready),
        "sign-in" => Some(Inbound::SignIn),
        "prepare" => file().map(Inbound::Prepare),
        "drag" => file().map(Inbound::Drag),
        _ => None,
    }
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// What a pump tick asks the loop to do once the entity borrow is released.
#[derive(Default)]
struct TickAction {
    /// Start an OS file drag with this file. Runs a modal loop, so never
    /// inside an update.
    drag: Option<PathBuf>,
    /// Keep pumping.
    keep_running: bool,
}

pub struct RigsPanel {
    owner: WeakEntity<StudioLayout>,
    remote: RemoteView,
    /// Body rect measured during the previous frame's prepaint, logical px.
    measured: Rc<Cell<Option<Bounds<Pixels>>>>,
    status: Status,
    /// Whether the tab wants the page on screen.
    wanted: bool,
    parent: Option<u64>,
    scale: f32,
    pending_rect: Option<ContentRect>,
    last_rect: Option<ContentRect>,
    view_id: Option<ViewId>,
    surface: Option<Surface>,
    /// Closed browsers whose native parent must outlive them until CEF has
    /// released it.
    retired: Vec<(ViewId, Surface)>,
    sync_scheduled: bool,
    pump_running: bool,
    /// Whether the page has been given the session that is current now, so a
    /// sign-in or sign-out in Studio reloads it.
    session_sent_for: Option<Option<String>>,
    downloads: HashMap<FileRef, Download>,
    /// A drag the page started, waiting for its file.
    pending_drag: Option<(FileRef, Instant)>,
    note: Option<String>,
    /// The page asked for Studio's sign-in dialog.
    sign_in_requested: bool,
}

impl RigsPanel {
    pub fn new(owner: Entity<StudioLayout>, _cx: &mut Context<Self>) -> Self {
        Self {
            owner: owner.downgrade(),
            remote: remote_view(),
            measured: Rc::new(Cell::new(None)),
            status: Status::Opening,
            wanted: false,
            parent: None,
            scale: 1.0,
            pending_rect: None,
            last_rect: None,
            view_id: None,
            surface: None,
            retired: Vec::new(),
            sync_scheduled: false,
            pump_running: false,
            session_sent_for: None,
            downloads: HashMap::new(),
            pending_drag: None,
            note: None,
            sign_in_requested: false,
        }
    }

    /// Take the page off screen: the tab or the sidebar went away. Safe to
    /// call from any update; the native work happens on the next tick.
    pub fn request_detach(&mut self, cx: &mut Context<Self>) {
        if !self.wanted && self.view_id.is_none() {
            return;
        }
        self.wanted = false;
        self.schedule_sync(cx);
    }

    fn schedule_sync(&mut self, cx: &mut Context<Self>) {
        if self.sync_scheduled {
            return;
        }
        self.sync_scheduled = true;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(16)).await;
            let _ = this.update(cx, |this, cx| {
                this.sync_scheduled = false;
                this.perform_sync(cx);
            });
        })
        .detach();
    }

    /// Create, move or close the browser to match what the tab wants. Only
    /// queues CEF work (see `builtin_plugin_editor::open_remote_view`); the
    /// pump does it.
    fn perform_sync(&mut self, cx: &mut Context<Self>) {
        if !self.wanted {
            self.detach();
            cx.notify();
            return;
        }
        let (Some(rect), Some(parent)) = (self.pending_rect, self.parent) else {
            return;
        };
        if self.surface.is_none() {
            let Some(surface) = surface_create(parent, rect) else {
                self.status = Status::Failed("could not create the view's window".into());
                cx.notify();
                return;
            };
            let view_id = host::allocate_view_id();
            let fill = ViewRect {
                x: 0,
                y: 0,
                width: rect.width,
                height: rect.height,
            };
            match host::open_remote_view(
                view_id,
                &self.remote,
                surface_handle(&surface),
                fill,
                self.scale,
            ) {
                Ok(()) => {
                    self.view_id = Some(view_id);
                    self.surface = Some(surface);
                    self.last_rect = Some(rect);
                    self.status = Status::Opening;
                    self.session_sent_for = None;
                    self.ensure_pump(cx);
                }
                Err(error) => {
                    self.status = Status::Failed(error.to_string());
                }
            }
            cx.notify();
            return;
        }
        if self.last_rect != Some(rect) {
            if let (Some(surface), Some(view_id)) = (self.surface.as_ref(), self.view_id) {
                surface_set_bounds(surface, parent, rect);
                host::set_view_bounds(
                    view_id,
                    ViewRect {
                        x: 0,
                        y: 0,
                        width: rect.width,
                        height: rect.height,
                    },
                    self.scale,
                );
            }
            self.last_rect = Some(rect);
        }
    }

    /// Close the browser. Its native parent is kept until CEF lets go of it:
    /// destroying a parent under a live browser is what `DESIGN.md` forbids.
    fn detach(&mut self) {
        if let Some(view_id) = self.view_id.take() {
            host::close_view(view_id);
            if let Some(surface) = self.surface.take() {
                self.retired.push((view_id, surface));
            }
        }
        self.surface = None;
        self.last_rect = None;
        self.pending_drag = None;
        self.session_sent_for = None;
        self.status = Status::Opening;
    }

    /// Drive the CEF pump and the bridge while a browser is open or closing.
    fn ensure_pump(&mut self, cx: &mut Context<Self>) {
        if self.pump_running {
            return;
        }
        self.pump_running = true;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor.timer(Duration::from_millis(16)).await;
                // CEF may dispatch native messages that re-enter GPUI, so it
                // runs with no entity borrowed — the editors' rule.
                host::pump();
                let Ok(action) = this.update(cx, |this, cx| this.tick(cx)) else {
                    break;
                };
                if let Some(path) = action.drag {
                    // A modal drag loop: also outside every borrow.
                    match os_file_drag::drag_file(&path) {
                        Ok(DragOutcome::Dropped | DragOutcome::Cancelled) => {}
                        Err(error) => {
                            let _ = this.update(cx, |this, cx| {
                                this.note = Some(format!("Could not drag the file: {error}"));
                                cx.notify();
                            });
                        }
                    }
                }
                if !action.keep_running {
                    let _ = this.update(cx, |this, _| this.pump_running = false);
                    break;
                }
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) -> TickAction {
        let mut action = TickAction::default();

        // Release native parents CEF no longer holds.
        self.retired
            .retain(|(view_id, _)| host::browser_holds_native_parent(*view_id));

        let Some(view_id) = self.view_id else {
            action.keep_running = !self.retired.is_empty();
            return action;
        };
        action.keep_running = true;

        // Space pressed in the page outside a text field is the transport's,
        // as in a plug-in editor.
        for _ in 0..host::take_global_play_pause_requests(view_id) {
            transport_key::claim(TransportKeySource::WebEditor, None);
        }

        for event in host::take_view_events(view_id) {
            match event {
                ViewEvent::Opened => {
                    self.status = Status::Attached;
                    cx.notify();
                }
                ViewEvent::OpenFailed(error) => {
                    self.status = Status::Failed(format!("Rigs could not be opened: {error}"));
                    self.detach();
                    cx.notify();
                    return action;
                }
                ViewEvent::RendererCrashed => {
                    // The host reloads the page; it will ask for the session
                    // again.
                    self.session_sent_for = None;
                }
                _ => {}
            }
        }

        // Signing in or out in Studio changes what the page should show.
        let token = crate::auth::session_token();
        if let Some(sent) = &self.session_sent_for {
            if *sent != token {
                self.session_sent_for = None;
                host::reload_view(view_id);
            }
        }

        for body in host::take_inbound(TAG) {
            match parse_inbound(&body) {
                Some(Inbound::Ready) => self.send_session(view_id, token.clone()),
                Some(Inbound::SignIn) => self.request_sign_in(cx),
                Some(Inbound::Prepare(file)) => self.start_download(file, cx),
                Some(Inbound::Drag(file)) => {
                    self.start_download(file.clone(), cx);
                    self.pending_drag = Some((file, Instant::now()));
                    self.note = None;
                }
                None => {}
            }
        }

        if let Some((file, started)) = self.pending_drag.clone() {
            match self.downloads.get(&file) {
                Some(Download::Ready(path)) => {
                    self.pending_drag = None;
                    if os_file_drag::SUPPORTED {
                        if os_file_drag::primary_button_down() {
                            action.drag = Some(path.clone());
                        }
                        // Released before the file arrived: the person let
                        // go, so nothing follows the pointer.
                    } else {
                        self.add_to_project(path.clone(), cx);
                    }
                }
                Some(Download::Failed(error)) => {
                    self.pending_drag = None;
                    self.note = Some(error.clone());
                    cx.notify();
                }
                _ if started.elapsed() > DRAG_WAIT
                    || (os_file_drag::SUPPORTED && !os_file_drag::primary_button_down()) =>
                {
                    self.pending_drag = None;
                }
                _ => {}
            }
        }
        action
    }

    /// Answer the page's handshake with Studio's session, posted only to the
    /// Rigs origin.
    fn send_session(&mut self, view_id: ViewId, token: Option<String>) {
        let profile = crate::auth::current_profile();
        let message = serde_json::json!({
            "type": "futureboard-host:session",
            "token": token,
            "name": profile.as_ref().and_then(|p| p.username.clone()).unwrap_or_default(),
            "avatarUrl": profile.as_ref().and_then(|p| p.avatar_url.clone()).unwrap_or_default(),
        });
        let origin = serde_json::Value::String(self.remote.origin.clone());
        host::send_to_view(
            view_id,
            &format!("window.postMessage({message}, {origin});"),
        );
        self.session_sent_for = Some(token);
    }

    /// The account dialog needs the studio window, which a pump tick does not
    /// have; the next render (which does) opens it.
    fn request_sign_in(&mut self, cx: &mut Context<Self>) {
        self.sign_in_requested = true;
        cx.notify();
    }

    /// Fetch a file into the Rigs download folder, once.
    fn start_download(&mut self, file: FileRef, cx: &mut Context<Self>) {
        if matches!(
            self.downloads.get(&file),
            Some(Download::Pending | Download::Ready(_))
        ) {
            return;
        }
        let Some(url) = file.url(&self.remote.origin) else {
            self.downloads
                .insert(file, Download::Failed("not a Rigs file".into()));
            return;
        };
        self.downloads.insert(file.clone(), Download::Pending);
        let token = crate::auth::session_token();
        let slug = file.slug.clone();
        let task = cx
            .background_executor()
            .spawn(async move { download(url, token, &slug) });
        let key = file;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, _| {
                this.downloads.insert(
                    key,
                    match result {
                        Ok(path) => Download::Ready(path),
                        Err(error) => Download::Failed(error),
                    },
                );
            });
        })
        .detach();
    }

    /// Where a file drag is not possible: add the download to the project.
    fn add_to_project(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let owner = self.owner.clone();
        cx.defer(move |cx| {
            if let Some(owner) = owner.upgrade() {
                owner.update(cx, |layout, cx| layout.add_rigs_download(&path, cx));
            }
        });
    }
}

/// Download `url` (bearer `token` for a private share) into
/// `Samples/Rigs/<slug>/` (audio) or `Presets/Rigs/` (a `.pst`), named as the
/// server names it. Blocking: runs on the background executor.
fn download(url: reqwest::Url, token: Option<String>, slug: &str) -> Result<PathBuf, String> {
    use std::io::Read;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?;
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|_| "Could not reach Rigs. Check your connection.".to_string())?;
    if !response.status().is_success() {
        return Err(format!("Rigs refused the download ({})", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_DOWNLOAD_BYTES)
    {
        return Err("The file is larger than Rigs serves.".into());
    }
    let name = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(disposition_file_name)
        .map(|name| safe_file_name(&name))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("{}.bin", safe_file_name(slug)));

    let paths = crate::paths::FutureboardPaths::resolve();
    let folder = if name.to_ascii_lowercase().ends_with(".pst") {
        paths.presets.join("Rigs")
    } else {
        paths.samples.join("Rigs").join(safe_file_name(slug))
    };
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let target = folder.join(&name);
    let partial = folder.join(format!(".{name}.part"));

    let mut body = response.take(MAX_DOWNLOAD_BYTES + 1);
    let mut file = std::fs::File::create(&partial).map_err(|error| error.to_string())?;
    let written = std::io::copy(&mut body, &mut file).map_err(|error| error.to_string())?;
    drop(file);
    if written > MAX_DOWNLOAD_BYTES {
        let _ = std::fs::remove_file(&partial);
        return Err("The file is larger than Rigs serves.".into());
    }
    std::fs::rename(&partial, &target).map_err(|error| error.to_string())?;
    Ok(target)
}

/// The file name in a `Content-Disposition` header, preferring the UTF-8
/// `filename*` form the Rigs API sends.
fn disposition_file_name(header: &str) -> Option<String> {
    for part in header.split(';').map(str::trim) {
        if let Some(encoded) = part.strip_prefix("filename*=UTF-8''") {
            return percent_decode(encoded);
        }
    }
    header
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("filename="))
        .map(|name| name.trim_matches('"').to_string())
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A name that stays inside the folder it is written to: no separators, no
/// parent references, nothing Windows refuses.
fn safe_file_name(name: &str) -> String {
    // The last segment after any separator. Not `Path::file_name`: on Windows
    // that reads `a:` as a drive and drops it.
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if cleaned == "." || cleaned == ".." {
        String::new()
    } else {
        cleaned
    }
}

impl Render for RigsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !embedding_supported() {
            return unavailable(
                "Rigs opens in your browser on this platform.",
                Some(self.remote.origin.clone()),
            )
            .into_any_element();
        }

        if std::mem::take(&mut self.sign_in_requested) {
            window.defer(cx, |window, cx| {
                crate::account::dispatch_account_action(
                    crate::account::AccountAction::SignIn,
                    window,
                    cx,
                );
            });
        }

        self.wanted = true;
        self.parent = native_window_handle(window);
        self.scale = window.scale_factor();
        self.pending_rect = self
            .measured
            .get()
            .and_then(|bounds| embed_rect(bounds, self.scale));
        if self.pending_rect.is_some()
            && (self.view_id.is_none() || self.pending_rect != self.last_rect)
            && !matches!(self.status, Status::Failed(_))
        {
            self.schedule_sync(cx);
        }

        let measured = Rc::clone(&self.measured);
        let attached = self.status == Status::Attached;
        let overlay = match &self.status {
            Status::Attached => None,
            Status::Opening => Some("Opening Rigs…".to_string()),
            Status::Failed(reason) => Some(reason.clone()),
        };
        let failed = matches!(self.status, Status::Failed(_));
        let note = self.note.clone();

        let mut root = div().size_full().flex().flex_col();
        if !attached {
            // Opaque until the browser covers the region; after that GPUI
            // cannot paint over the child window anyway (WS_CLIPCHILDREN).
            root = root.bg(Colors::surface_panel());
        }
        root.on_children_prepainted(move |bounds, _window, cx| {
            let Some(bounds) = bounds.first().copied() else {
                return;
            };
            let changed = measured.get().is_none_or(|previous| {
                (f32::from(previous.origin.x) - f32::from(bounds.origin.x)).abs() > 0.5
                    || (f32::from(previous.origin.y) - f32::from(bounds.origin.y)).abs() > 0.5
                    || (f32::from(previous.size.width) - f32::from(bounds.size.width)).abs() > 0.5
                    || (f32::from(previous.size.height) - f32::from(bounds.size.height)).abs() > 0.5
            });
            if changed {
                measured.set(Some(bounds));
                cx.refresh_windows();
            }
        })
        .child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(space::SNUG))
                .px(px(space::BASE))
                .children(overlay.map(|text| {
                    div()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(text)
                }))
                .children(failed.then(|| {
                    div()
                        .id("rigs-retry")
                        .px(px(space::BASE))
                        .py(px(space::TIGHT))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .border(px(1.0))
                        .border_color(Colors::border_subtle())
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_primary())
                        .cursor(gpui::CursorStyle::PointingHand)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.status = Status::Opening;
                            this.schedule_sync(cx);
                            cx.notify();
                        }))
                        .child("Try again")
                })),
        )
        .children(note.map(|text| {
            // A download or drag that failed: said once, under the page.
            div()
                .flex_shrink_0()
                .px(px(space::BASE))
                .py(px(space::TIGHT))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(text)
        }))
        .into_any_element()
    }
}

/// The measured logical rect, as the physical window-relative rect a native
/// child is placed with. `None` while collapsed.
fn embed_rect(bounds: Bounds<Pixels>, scale: f32) -> Option<ContentRect> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let rect = ContentRect {
        x: (f32::from(bounds.origin.x) * scale).round() as i32,
        y: (f32::from(bounds.origin.y) * scale).round() as i32,
        width: (f32::from(bounds.size.width) * scale).round() as i32,
        height: (f32::from(bounds.size.height) * scale).round() as i32,
    };
    (rect.width >= MIN_EMBED_PX && rect.height >= MIN_EMBED_PX).then_some(rect)
}

fn unavailable(message: &str, link: Option<String>) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(space::SNUG))
        .px(px(space::BASE))
        .bg(Colors::surface_panel())
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(message.to_string()),
        )
        .children(link.map(|url| {
            div()
                .id("rigs-open-browser")
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_primary())
                .cursor(gpui::CursorStyle::PointingHand)
                .on_click(move |_, _, cx| cx.open_url(&url))
                .child("Open rigs.futureboard.studio")
        }))
}

#[cfg(target_os = "windows")]
fn native_window_handle(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get() as u64),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn native_window_handle(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::AppKit(w) => Some(w.ns_view.as_ptr() as u64),
        _ => None,
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn native_window_handle(_window: &Window) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_messages_parse_and_reject_junk() {
        assert_eq!(parse_inbound(br#"{"type":"ready"}"#), Some(Inbound::Ready));
        assert_eq!(
            parse_inbound(br#"{"type":"sign-in"}"#),
            Some(Inbound::SignIn)
        );
        assert_eq!(
            parse_inbound(br#"{"type":"drag","slug":"glass-pad","label":"x"}"#),
            Some(Inbound::Drag(FileRef {
                slug: "glass-pad".into(),
                file_id: None
            }))
        );
        assert_eq!(
            parse_inbound(
                br#"{"type":"prepare","slug":"kit","fileId":"6DD39510-A72C-4B2F-92E4-C467C165F408"}"#
            ),
            Some(Inbound::Prepare(FileRef {
                slug: "kit".into(),
                file_id: Some("6dd39510-a72c-4b2f-92e4-c467c165f408".into())
            }))
        );
        for junk in [
            &br#"{"type":"drag","slug":""}"#[..],
            br#"{"type":"drag","slug":"x","fileId":"../../etc"}"#,
            br#"{"type":"drag"}"#,
            br#"{"type":"exec","code":"x"}"#,
            b"not json",
        ] {
            assert_eq!(
                parse_inbound(junk),
                None,
                "{}",
                String::from_utf8_lossy(junk)
            );
        }
    }

    /// The URL is built from the origin and escaped segments only: a slug
    /// cannot climb out of `/rigs/` or reach another host.
    #[test]
    fn download_urls_stay_on_the_rigs_api() {
        let preset = FileRef {
            slug: "glass-pad".into(),
            file_id: None,
        };
        assert_eq!(
            preset.url(RIGS_ORIGIN).unwrap().as_str(),
            "https://rigs.futureboard.studio/rigs/glass-pad/download"
        );
        let hostile = FileRef {
            slug: "../../evil?x=1#y".into(),
            file_id: Some("6dd39510-a72c-4b2f-92e4-c467c165f408".into()),
        };
        let url = hostile.url(RIGS_ORIGIN).unwrap();
        assert_eq!(url.host_str(), Some("rigs.futureboard.studio"));
        assert!(url.path().starts_with("/rigs/"));
        assert!(url.query().is_none() && url.fragment().is_none());
        let thai = FileRef {
            slug: "แทร็กเสียง-1-chain".into(),
            file_id: None,
        };
        assert!(thai
            .url(RIGS_ORIGIN)
            .unwrap()
            .path()
            .starts_with("/rigs/%E0%B9%81"));
    }

    #[test]
    fn download_names_come_from_the_header_and_stay_in_the_folder() {
        assert_eq!(
            disposition_file_name(
                r#"attachment; filename="____ _1_.wav"; filename*=UTF-8''%E0%B8%81%E0%B8%A5%E0%B8%AD%E0%B8%87.wav"#
            )
            .as_deref(),
            Some("กลอง.wav")
        );
        assert_eq!(
            disposition_file_name(r#"attachment; filename="kit.zip""#).as_deref(),
            Some("kit.zip")
        );
        assert_eq!(safe_file_name("../../evil.pst"), "evil.pst");
        assert_eq!(safe_file_name(r"..\..\evil.wav"), "evil.wav");
        assert_eq!(safe_file_name("a:b*c?.wav"), "a_b_c_.wav");
        assert_eq!(safe_file_name(".."), "");
    }
}
