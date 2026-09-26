use std::time::{Duration, Instant};

use gpui::{App, Bounds, Context, Window};

use crate::components::native_editor_shell::{NativeEditorShell, shell_defaults};
use crate::components::plugin_manager::open_plugin_manager_window;
use crate::components::plugin_picker::{
    PickerFilter, PluginInsertKind, PluginPickerState, STUB_PLUGIN_ID, ensure_default_highlight,
};
use crate::components::timeline::timeline_state::{PluginRuntimeBackend, PluginRuntimeState};
use crate::components::transport_key::{self, TransportKeySource};
use crate::layout::plugin_editor_chrome_ops::is_ara_editor_key;
use SpherePluginHost::{CatalogLoad, load_au_cache_state};

use super::{PluginCatalogStatus, PluginSearchIndex, StudioLayout};

/// Wall-clock milliseconds since the Unix epoch, used to stamp the built-in
/// catalog rows so they sort/merge alongside scanned plug-ins.
fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Plugin catalog / registry-scan state backing the insert picker — the cached
/// scan result, whether the preset cache exists on disk, and the catalog load
/// phase. `StudioLayout` decomposition slice (manual `Default`: status=Loading).
pub(crate) struct PluginCatalogState {
    /// Cached plugin registry scan result; `None` until the first scan.
    pub available: Option<Vec<SpherePluginHost::RegistryPlugin>>,
    /// `true` if the cached preset directory exists on disk.
    pub cache_present: bool,
    /// Catalog load phase (Loading / Ready / …) driving the picker skeleton/error UI.
    pub status: PluginCatalogStatus,
}

impl Default for PluginCatalogState {
    fn default() -> Self {
        Self {
            available: None,
            cache_present: false,
            status: PluginCatalogStatus::Loading,
        }
    }
}

/// Plugin-editor window handles owned by the studio — the GPUI-hosted editor
/// shells, the native external-bridge editor sessions, the shared bridge
/// runtime, and editor opens deferred while an insert runtime was still loading.
/// `StudioLayout` decomposition slice (every field is `Default`).
#[derive(Default)]
pub(crate) struct PluginEditorWindows {
    /// Open native plugin editor windows keyed by `(track_id, insert_id)` →
    /// GPUI-hosted editor window handle (GPUI borderless shell, native VST3
    /// child region; dropping the entity detaches the view).
    pub open: std::collections::HashMap<
        (String, String),
        gpui::WindowHandle<crate::components::plugin_editor_window::PluginEditorWindow>,
    >,
    /// Open built-in plugin editor windows keyed by `plugin_id` — one shared
    /// CEF browser per built-in plugin type, not one per insert. Many
    /// track/insert DSP instances of the same `plugin_id` bind to the same
    /// window; the window's own sidebar tracks which one is active. See
    /// `open_builtin_insert_editor`.
    pub builtin: std::collections::HashMap<
        String,
        gpui::WindowHandle<
            crate::components::builtin_plugin_editor_window::BuiltinPluginEditorWindow,
        >,
    >,
    /// Native main-owned external-bridge editor shells, keyed by
    /// `(track_id, plugin_instance_id)`.
    pub bridge: std::collections::HashMap<(String, String), BridgeEditorSession>,
    /// Shared external-bridge plugin runtime, if active.
    pub bridge_runtime: Option<super::plugin_bridge_runtime::SharedPluginBridgeRuntime>,
    /// Editor opens requested while the insert runtime was still loading.
    pub deferred_opens: Vec<(String, usize, String)>,
    /// Loop guard: consecutive per-frame `flush` attempts per instance. Reset
    /// to 0 the moment an instance stops being re-queued. If it ever climbs past
    /// the cap, the editor open is forced terminal (spec `[EDITOR_LOOP_GUARD]`)
    /// so a re-queue source can never spin forever.
    pub flush_attempts: std::collections::HashMap<String, u32>,
    /// Plug-in instances the host is still loading, as
    /// `(plugin_instance_id, display_name)` in the order they were added.
    /// Drives the indeterminate load dialog; empty closes it.
    pub loading: Vec<(String, String)>,
    /// Which stored preset the editor chrome is sitting on, per insert.
    ///
    /// Not persisted and not the plug-in's own state: it is only which entry of
    /// the preset list the user last stepped to, so the strip can name it.
    pub preset_selection: std::collections::HashMap<(String, String), usize>,
    /// Inserts with an editor tab open, per channel.
    ///
    /// One window per channel, one tab per plug-in the user opened in it. This
    /// is the list the tab strip is built from; `open` holds the window itself,
    /// still keyed by whichever insert opened it first.
    pub editor_tabs: std::collections::HashMap<String, Vec<String>>,
    /// Plug-in state edits grouped into gestures for the project's dirty flag.
    /// See [`StudioLayout::note_plugin_state_edited`].
    pub state_edits: PluginEditGesture,
    /// State the editor chrome's Copy took, for its Paste. In-app only and
    /// not persisted: opaque state means nothing outside the plug-in it came
    /// from, so it is never offered to the system clipboard.
    pub state_clipboard: Option<PluginStateClipboard>,
    /// A channel's effects as "Copy FX Chain" took them, for "Paste FX
    /// Chain". In-app and not persisted, like `state_clipboard`.
    pub fx_chain_clipboard: Option<FxChainClipboard>,
}

/// One channel's copied effects, in chain order. Each slot is the original's
/// with its state as captured at the copy; a paste lands new instances of
/// them (`TimelineState::replace_fx_chain`), never these ids.
#[derive(Clone, Debug)]
pub(crate) struct FxChainClipboard {
    pub source_name: String,
    pub effects: Vec<crate::components::timeline::timeline_state::InsertSlotState>,
}

/// One plug-in's copied state. Pasted only onto an insert of the same
/// `plugin_id`: state handed to another plug-in is corruption, not a preset.
#[derive(Clone, Debug)]
pub(crate) struct PluginStateClipboard {
    pub plugin_id: String,
    pub plugin_name: String,
    pub state: std::sync::Arc<Vec<u8>>,
}

/// How long a plug-in edit gesture may pause and still be the same gesture.
///
/// Longer than the plug-in host's report interval
/// (`SpherePluginHost::state_touch::REPORT_INTERVAL`), so a knob drag in a
/// bridged editor stays one gesture, and short enough that the trailing mark
/// lands soon after the user lets go.
pub(crate) const PLUGIN_EDIT_GESTURE_GAP: Duration = Duration::from_millis(750);

/// Plug-in state edits grouped into gestures for the project's dirty flag.
///
/// Every edit has to leave the project dirty, including one made while a save
/// writes a snapshot taken before it: the session's `dirty_generation` must move
/// after that snapshot, or the finished save reports the edit as saved. Moving
/// it on every frame of a knob drag is wasted work, so a gesture — edits no more
/// than [`PLUGIN_EDIT_GESTURE_GAP`] apart, in one session — marks the project
/// when it starts, and once more when it ends if anything arrived after that
/// first mark. Pure; `StudioLayout` drives it and owns the timer.
#[derive(Debug, Default)]
pub(crate) struct PluginEditGesture {
    /// The latest edit and the session generation it belonged to.
    last_edit: Option<(Instant, u64)>,
    /// An edit arrived after the gesture's opening mark.
    unmarked: bool,
    /// A settle check is scheduled.
    settle_armed: bool,
}

/// What one edit asks of the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PluginEditStep {
    /// Mark the project dirty now (the edit opened a gesture).
    pub mark: bool,
    /// Schedule [`PluginEditGesture::settle`] after [`PLUGIN_EDIT_GESTURE_GAP`].
    pub arm_settle: bool,
}

/// The answer of a settle check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginEditSettle {
    /// The gesture is still going: check again after this long.
    Wait(Duration),
    /// The gesture ended. When edits followed its opening mark, mark the
    /// project once more — if it is still this session.
    Done { mark_session: Option<u64> },
}

impl PluginEditGesture {
    pub(crate) fn edit(&mut self, now: Instant, session: u64) -> PluginEditStep {
        let continues = self.last_edit.is_some_and(|(last, last_session)| {
            last_session == session
                && now.saturating_duration_since(last) <= PLUGIN_EDIT_GESTURE_GAP
        });
        self.last_edit = Some((now, session));
        self.unmarked = continues;
        let arm_settle = !self.settle_armed;
        self.settle_armed = true;
        PluginEditStep {
            mark: !continues,
            arm_settle,
        }
    }

    pub(crate) fn settle(&mut self, now: Instant) -> PluginEditSettle {
        let Some((last, session)) = self.last_edit else {
            self.settle_armed = false;
            return PluginEditSettle::Done { mark_session: None };
        };
        let quiet = now.saturating_duration_since(last);
        if quiet <= PLUGIN_EDIT_GESTURE_GAP {
            return PluginEditSettle::Wait(
                PLUGIN_EDIT_GESTURE_GAP - quiet + Duration::from_millis(1),
            );
        }
        self.settle_armed = false;
        let mark_session = std::mem::take(&mut self.unmarked).then_some(session);
        PluginEditSettle::Done { mark_session }
    }
}

impl PluginEditorWindows {
    fn discard_pending_open(&mut self, instance_id: &str) {
        self.deferred_opens.retain(|(_, _, id)| id != instance_id);
        self.flush_attempts.remove(instance_id);
    }
}

/// `FUTUREBOARD_PLUGIN_EDITOR_DEBUG=1` gates the structured editor-lifecycle
/// logs (open request / result / failure / timing). These fire only on state
/// transitions — never from the paint loop or the audio callback.
pub(crate) fn plugin_editor_debug() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_EDITOR_DEBUG").is_some())
}

/// Structured editor log, gated by [`plugin_editor_debug`]. Prefix `[plugin-editor]`.
macro_rules! ped_log {
    ($($arg:tt)*) => {
        if plugin_editor_debug() {
            eprintln!("[plugin-editor] {}", format_args!($($arg)*));
        }
    };
}

/// Editor open watchdog (spec A6). A bridge session that has not reached
/// `Attached` within this window is marked `Failed` so the next Open click
/// retries instead of focusing a dead loading shell — the concrete "Plugin
/// Editor sometimes cannot open again" regression. The wrapper window lives in
/// the main process, so the user can always close a timed-out shell too.
const EDITOR_OPEN_TIMEOUT: Duration = Duration::from_secs(12);
/// Host-owned (detached) editors create the native view in the plugin-host
/// process. Heavy VSTi `createView`/`attached` on macOS routinely exceeds the
/// legacy 12s main-owned budget, so host-owned opens align with the host's
/// 30s create/attach hang watchdogs.
const HOST_OWNED_EDITOR_OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const EDITOR_FIRST_PAINT_TIMEOUT: Duration = Duration::from_secs(5);

fn editor_open_timeout() -> Duration {
    if bridge_editor_host_owned() {
        HOST_OWNED_EDITOR_OPEN_TIMEOUT
    } else {
        EDITOR_OPEN_TIMEOUT
    }
}

fn loading_plugin_status(display_name: &str) -> String {
    format!("Loading Plugin\n{display_name}")
}

/// Whether bridge editors are host-owned (default). In host-owned mode the
/// plugin-host process owns a detached top-level editor window and the GPUI
/// main app creates NO plugin window — nothing the foreign plugin view is ever
/// parented under, so the main UI thread can never be coupled to (and frozen
/// by) a slow/hanging plugin editor. The legacy main-owned `WS_CHILD` shell is
/// the inverse of this flag. Single source of truth shared with the host's
/// editor-mode env (`sanitize_child_env`). No vendor/plugin branching.
fn bridge_editor_host_owned() -> bool {
    !SpherePluginHost::plugin_host_client::editor_main_owned_shell_enabled()
}

/// Lifecycle state of a native main-owned bridge editor session.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BridgeEditorState {
    /// Shell is visible while the plugin instance is still loading in the host.
    Loading,
    /// Main-owned shell/content HWND exists and is visible.
    ParentWindowCreated,
    /// `PrepareEditorView` sent; awaiting `EditorPreferredSize`.
    ViewCreated,
    /// Shell resized to preferred size; `ConfirmEditorContentReady` sent.
    Sized,
    /// Host has the final content HWND and is attaching the VST3 view.
    AwaitingAttach,
    /// `IPlugView` attached into the native content HWND.
    Attached,
    /// Content HWND has painted after attach.
    Visible,
    /// Editor attach and first-paint watchdogs completed.
    Ready,
    /// Attach failed / host disconnected.
    Failed(String),
    /// Attach or first paint timed out.
    TimedOut(String),
}

/// One open native main-owned plugin editor (external-bridge path). The window
/// is a real Win32 top-level shell (`NativeEditorShell`) owned by the main app;
/// the host process attaches the VST3 view into the shell's content HWND over
/// IPC. No GPUI surface is composited over it, so it actually paints (spec
/// Part 7). NOT `host_detached` — the shell is main-owned.
pub(crate) struct BridgeEditorSession {
    pub(crate) track_id: String,
    pub(crate) instance_id: String,
    pub(crate) display_name: String,
    pub(crate) shell: NativeEditorShell,
    pub(crate) state: BridgeEditorState,
    /// True once the plug-in's preferred size has been applied to the shell.
    pub(crate) preferred_applied: bool,
    /// Last content (client) size pushed to the host as `ResizeEditor`.
    pub(crate) last_content: (i32, i32),
    /// Plugin-host child HWND reported in `EditorAttached` (0 until attached).
    pub(crate) host_hwnd: u64,
    /// When the open request was issued. Drives the open watchdog
    /// ([`EDITOR_OPEN_TIMEOUT`]) and the request→attach timing logs (spec A4).
    pub(crate) requested_at: Instant,
    /// When `EditorAttached` arrived. Drives the first-paint watchdog.
    pub(crate) attached_at: Option<Instant>,
    /// Content WM_PAINT count sampled at attach.
    pub(crate) paint_count_at_attach: u32,
    /// True once `[EDITOR FIRST PAINT]` has been emitted.
    pub(crate) first_paint_logged: bool,
}

/// Logical→physical DPI passthrough for `ResizeEditor`. The host sizes the view
/// from the actual child client rect, so this value is a hint only.
fn bridge_editor_dpi(session: &BridgeEditorSession) -> u32 {
    session.shell.shell_dpi()
}

fn bridge_editor_state_name(state: &BridgeEditorState) -> &'static str {
    match state {
        BridgeEditorState::Loading => "Opening",
        BridgeEditorState::ParentWindowCreated => "ParentWindowCreated",
        BridgeEditorState::ViewCreated => "ViewCreated",
        BridgeEditorState::Sized => "Sized",
        BridgeEditorState::AwaitingAttach => "AwaitingAttach",
        BridgeEditorState::Attached => "Attached",
        BridgeEditorState::Visible => "Visible",
        BridgeEditorState::Ready => "Ready",
        BridgeEditorState::Failed(_) => "Failed",
        BridgeEditorState::TimedOut(_) => "TimedOut",
    }
}

fn transition_bridge_editor_state(
    session: &mut BridgeEditorSession,
    new_state: BridgeEditorState,
    reason: &str,
) {
    let from = bridge_editor_state_name(&session.state);
    let to = bridge_editor_state_name(&new_state);
    if from != to {
        eprintln!(
            "[EDITOR STATE TRANSITION]\nplugin_instance_id={}\nfrom={from}\nto={to}\nreason={reason}\nelapsed_ms={}",
            session.instance_id,
            session.requested_at.elapsed().as_millis()
        );
    }
    session.state = new_state;
}

fn bridge_editor_is_open(state: &BridgeEditorState) -> bool {
    matches!(
        state,
        BridgeEditorState::Attached | BridgeEditorState::Visible | BridgeEditorState::Ready
    )
}

fn bridge_editor_is_terminal(state: &BridgeEditorState) -> bool {
    matches!(
        state,
        BridgeEditorState::Failed(_) | BridgeEditorState::TimedOut(_)
    )
}

/// What an Open-Editor request should do when a session already exists for the
/// same plugin instance (spec A6 re-open semantics). Pure so the "a Failed /
/// timed-out session is never treated as live" contract is unit-tested and
/// cannot silently regress into focusing a dead loading shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorReopenAction {
    /// Live or still-in-flight (attaching) — focus it; never spawn a duplicate.
    FocusExisting,
    /// Still `Loading` in the host — reuse the existing loading shell.
    ReuseLoadingShell,
    /// Terminal (`Failed` / `TimedOut`) — drop the stale session and open fresh.
    DropAndRetry,
}

fn editor_reopen_action(state: &BridgeEditorState) -> EditorReopenAction {
    if bridge_editor_is_terminal(state) {
        // Terminal first: a Failed/TimedOut session must never be focused as if
        // it were live — that was the "cannot open the editor again" bug.
        EditorReopenAction::DropAndRetry
    } else if matches!(state, BridgeEditorState::Loading) {
        EditorReopenAction::ReuseLoadingShell
    } else {
        EditorReopenAction::FocusExisting
    }
}

impl StudioLayout {
    /// A plug-in's own state changed: a built-in editor forwarded a parameter,
    /// or the plug-in host reported edits in a bridged plug-in's editor.
    ///
    /// Marks the project as needing a save, and nothing else: view-only dirty,
    /// because no engine graph changed (the edit already reached the DSP), and
    /// no state is copied, because the next save asks every plug-in for its
    /// real state. Coalesced per gesture ([`PluginEditGesture`]); the gesture's
    /// trailing mark runs on a timer so an edit made during a save is not lost.
    pub(super) fn note_plugin_state_edited(&mut self, cx: &mut Context<Self>) {
        let session = self.session_generation();
        let step = self
            .plugin_editors
            .state_edits
            .edit(Instant::now(), session);
        if step.mark {
            self.mark_dirty_view_only();
            cx.notify();
        }
        if !step.arm_settle {
            return;
        }
        cx.spawn(async move |this, cx| {
            let mut wait = PLUGIN_EDIT_GESTURE_GAP;
            loop {
                cx.background_executor().timer(wait).await;
                let next = this.update(cx, |this, cx| {
                    match this.plugin_editors.state_edits.settle(Instant::now()) {
                        PluginEditSettle::Wait(remaining) => Some(remaining),
                        PluginEditSettle::Done { mark_session } => {
                            if mark_session == Some(this.session_generation()) {
                                this.mark_dirty_view_only();
                                cx.notify();
                            }
                            None
                        }
                    }
                });
                match next {
                    Ok(Some(remaining)) => wait = remaining,
                    Ok(None) | Err(_) => break,
                }
            }
        })
        .detach();
    }

    pub(super) fn poll_plugin_bridge_runtime(&mut self, cx: &mut Context<Self>) {
        use crate::components::timeline::timeline_state::{
            PluginRuntimeBackend, PluginRuntimeState,
        };
        use SpherePluginHost::ipc::HostEvent;
        use SpherePluginHost::plugin_host_client::ClientEvent;

        let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref().cloned() else {
            return;
        };
        let events = runtime
            .lock()
            .map(|mut runtime| runtime.drain_events())
            .unwrap_or_default();
        if events.is_empty() {
            return;
        }
        let mut changed = false;
        // This poll is the SINGLE drain of the shared runtime queue. Editor-
        // targeted events (EditorAttached / EditorPreferredSize / …) must be
        // forwarded to the owning editor window, or they are lost and the editor
        // stays stuck on "Loading" (spec Part 2/5/6). Collect them while we
        // handle the load-lifecycle events, then dispatch after the loop so we
        // never hold a borrow of `self` across `handle.update`.
        let mut editor_routes: Vec<(String, ClientEvent)> = Vec::new();
        let mut disconnect_all = false;
        for event in events {
            if let Some(instance) =
                crate::components::plugin_editor_window::PluginEditorWindow::editor_event_instance_id(
                    &event,
                )
            {
                editor_routes.push((instance.to_string(), event.clone()));
            } else if matches!(event, ClientEvent::Disconnected) {
                disconnect_all = true;
            }
            match event {
                ClientEvent::Host(HostEvent::PluginLoading { plugin_instance_id }) => {
                    eprintln!("[plugin-bridge] event PluginLoading instance={plugin_instance_id}");
                    let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                    changed |= self.timeline.update(cx, |timeline, _cx| {
                        let track_ids = timeline
                            .state
                            .insert_owner_ids_containing(&plugin_instance_id);
                        track_ids.into_iter().any(|track_id| {
                            timeline.state.set_insert_runtime(
                                &track_id,
                                &plugin_instance_id,
                                PluginRuntimeBackend::ExternalBridge,
                                PluginRuntimeState::Loading,
                                host_pid,
                            )
                        })
                    });
                }
                ClientEvent::Host(HostEvent::PluginAlreadyLoaded {
                    plugin_instance_id,
                    name,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event PluginAlreadyLoaded instance={plugin_instance_id} name={name}"
                    );
                    eprintln!(
                        "[PluginRestore] reused runtime instance={plugin_instance_id} name={name}"
                    );
                    if let Ok(mut bridge) = runtime.lock() {
                        bridge.mark_plugin_loaded(&plugin_instance_id);
                    }
                    self.end_plugin_load_progress(&plugin_instance_id, cx);
                    changed |= self.on_bridge_plugin_host_ready(
                        &plugin_instance_id,
                        &name,
                        &runtime,
                        cx,
                        "plugin_already_loaded",
                    );
                }
                ClientEvent::Host(HostEvent::PluginLoaded {
                    plugin_instance_id,
                    name,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event PluginLoaded instance={plugin_instance_id} name={name}"
                    );
                    eprintln!(
                        "[PluginRestore] loaded insert instance={plugin_instance_id} name={name}"
                    );
                    if let Ok(mut bridge) = runtime.lock() {
                        bridge.mark_plugin_loaded(&plugin_instance_id);
                    }
                    self.end_plugin_load_progress(&plugin_instance_id, cx);
                    changed |= self.on_bridge_plugin_host_ready(
                        &plugin_instance_id,
                        &name,
                        &runtime,
                        cx,
                        "plugin_loaded",
                    );
                }
                ClientEvent::Host(HostEvent::BuiltinNamCaptureResult {
                    plugin_instance_id,
                    ok,
                    name,
                    error,
                    receptive_field,
                    full_rig,
                    architecture,
                    family,
                    slimmable,
                    submodel_count,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event BuiltinNamCaptureResult instance={plugin_instance_id} ok={ok} name={name} error={error:?} family={family}"
                    );
                    // Route into whichever shared built-in editor is bound to
                    // this insert; the window checks the match itself.
                    for handle in self.plugin_editors.builtin.values() {
                        let _ = handle.update(cx, |editor, _window, _cx| {
                            editor.notify_nam_capture_result(
                                &plugin_instance_id,
                                ok,
                                &name,
                                error.as_deref(),
                                receptive_field,
                                full_rig,
                                &architecture,
                                &family,
                                slimmable,
                                submodel_count,
                            );
                        });
                    }
                }
                ClientEvent::Host(HostEvent::BuiltinIrResult {
                    plugin_instance_id,
                    ok,
                    name,
                    error,
                    frames,
                    latency_samples,
                    stereo,
                    truncated,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event BuiltinIrResult instance={plugin_instance_id} ok={ok} name={name} error={error:?}"
                    );
                    for handle in self.plugin_editors.builtin.values() {
                        let _ = handle.update(cx, |editor, _window, _cx| {
                            editor.notify_ir_load_result(
                                &plugin_instance_id,
                                ok,
                                &name,
                                error.as_deref(),
                                frames,
                                latency_samples,
                                stereo,
                                truncated,
                            );
                        });
                    }
                }
                ClientEvent::Host(HostEvent::PluginLoadFailed {
                    plugin_instance_id,
                    error,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event PluginLoadFailed instance={plugin_instance_id} error={error}"
                    );
                    if let Ok(mut bridge) = runtime.lock() {
                        bridge.mark_plugin_load_failed(&plugin_instance_id);
                    }
                    self.end_plugin_load_progress(&plugin_instance_id, cx);
                    if let Some(engine) = self.audio_bridge.engine.as_ref() {
                        let _ = engine.set_plugin_bridge_sink(plugin_instance_id.clone(), None);
                    }
                    let user_error = if error.contains("CPU") || error.contains("runtime") {
                        error.clone()
                    } else {
                        format!(
                            "Plugin failed to load. It may require a newer CPU instruction set \
                             or a missing runtime dependency. ({error})"
                        )
                    };
                    let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                    changed |= self.timeline.update(cx, |timeline, _cx| {
                        let track_ids = timeline
                            .state
                            .insert_owner_ids_containing(&plugin_instance_id);
                        track_ids.into_iter().fold(false, |acc, track_id| {
                            let runtime_changed = timeline.state.set_insert_runtime(
                                &track_id,
                                &plugin_instance_id,
                                PluginRuntimeBackend::ExternalBridge,
                                PluginRuntimeState::Failed(user_error.clone()),
                                host_pid,
                            );
                            // Terminal: a queued editor open can never succeed for
                            // a plugin that failed to load. Clear it so `flush`
                            // stops retrying; the user must re-open deliberately.
                            timeline.state.set_insert_pending_editor_open(
                                &track_id,
                                &plugin_instance_id,
                                false,
                            );
                            acc || runtime_changed
                        })
                    });
                    self.plugin_editors
                        .deferred_opens
                        .retain(|(_, _, id)| id != &plugin_instance_id);
                    self.plugin_editors
                        .flush_attempts
                        .remove(&plugin_instance_id);
                    for session in self.plugin_editors.bridge.values_mut() {
                        if session.instance_id == plugin_instance_id
                            && !bridge_editor_is_terminal(&session.state)
                        {
                            session.shell.set_status("Plugin failed to load.", true);
                            transition_bridge_editor_state(
                                session,
                                BridgeEditorState::Failed("Plugin failed to load".to_string()),
                                "plugin_load_failed",
                            );
                        }
                    }
                }
                ClientEvent::Disconnected => {
                    eprintln!("[plugin-runtime] external bridge host disconnected");
                    // Nothing in flight will report back through a dead host.
                    self.cancel_all_plugin_load_progress(cx);
                }
                ClientEvent::Host(HostEvent::AudioBridgeConfigured {
                    sample_rate,
                    max_block_size,
                    follows_engine,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event AudioBridgeConfigured sample_rate={sample_rate} max_block_size={max_block_size} follows_engine={follows_engine}"
                    );
                }
                ClientEvent::Host(HostEvent::AudioBridgeStatus {
                    block_id,
                    dsp_output,
                    latency_samples,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event AudioBridgeStatus block_id={block_id} dsp_output={dsp_output} latency_samples={latency_samples}"
                    );
                }
                ClientEvent::Host(HostEvent::SharedAudioAttached {
                    attached,
                    name,
                    bytes,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event SharedAudioAttached attached={attached} name={name} bytes={bytes}"
                    );
                }
                ClientEvent::Host(HostEvent::ProcessingPrepared {
                    plugin_instance_id,
                    sample_rate,
                    max_block_size,
                    output_channels,
                    output_bus_channels,
                }) => {
                    eprintln!(
                        "[plugin-bridge] event ProcessingPrepared instance={plugin_instance_id} sr={sample_rate} block={max_block_size} outputs={output_channels} buses={output_bus_channels:?}"
                    );
                    eprintln!(
                        "[PluginRestore] setupProcessing sample_rate={sample_rate} block_size={max_block_size}"
                    );
                    eprintln!("[PluginRestore] setActive true result=ok");
                    eprintln!("[plugin-runtime] dsp_output=ready");
                    let host_pid = runtime.lock().ok().and_then(|mut r| {
                        r.mark_plugin_output_channels(&plugin_instance_id, output_channels);
                        r.host_pid()
                    });
                    let mut pending_opens = Vec::new();
                    let processing_changed = self.timeline.update(cx, |timeline, _cx| {
                        let track_ids = timeline
                            .state
                            .insert_owner_ids_containing(&plugin_instance_id);
                        track_ids.into_iter().any(|track_id| {
                            let runtime_changed = timeline.state.set_insert_runtime(
                                &track_id,
                                &plugin_instance_id,
                                PluginRuntimeBackend::ExternalBridge,
                                PluginRuntimeState::Active,
                                host_pid,
                            );
                            // Record the real per-bus output layout BEFORE building
                            // child strips so multi-out plugins get one strip per
                            // real bus (mono→stereo) instead of paired flat channels.
                            let layout_changed = timeline.state.set_insert_output_bus_layout(
                                &track_id,
                                &plugin_instance_id,
                                &output_bus_channels,
                            );
                            let outputs_changed =
                                timeline.state.auto_enable_detected_insert_outputs(
                                    &track_id,
                                    &plugin_instance_id,
                                    output_channels,
                                );
                            if let Some((index, true)) =
                                timeline.state.insert_slots(&track_id).and_then(|slots| {
                                    slots
                                        .iter()
                                        .enumerate()
                                        .find(|(_, slot)| slot.id == plugin_instance_id)
                                        .map(|(index, slot)| (index, slot.pending_open_editor))
                                })
                            {
                                timeline.state.set_insert_pending_editor_open(
                                    &track_id,
                                    &plugin_instance_id,
                                    false,
                                );
                                pending_opens.push((
                                    track_id.clone(),
                                    index,
                                    plugin_instance_id.clone(),
                                ));
                            }
                            runtime_changed || layout_changed || outputs_changed
                        })
                    });
                    changed |= processing_changed;
                    self.plugin_editors.deferred_opens.extend(pending_opens);
                    self.sync_plugin_bridge_sinks_to_engine(cx, "processing_prepared");
                    if processing_changed {
                        self.audio_bridge.project_dirty = true;
                        self.schedule_audio_project_sync(cx, true, "bridge_processing_prepared");
                    }
                    self.request_bridge_insert_parameters(&plugin_instance_id);
                }
                ClientEvent::Host(HostEvent::PluginParameters {
                    plugin_instance_id,
                    ok,
                    parameters,
                }) => {
                    if ok {
                        changed |= self.apply_bridge_insert_parameters(
                            &plugin_instance_id,
                            parameters,
                            cx,
                        );
                    } else {
                        eprintln!(
                            "[plugin-bridge] event PluginParameters failed instance={plugin_instance_id}"
                        );
                    }
                }
                // A bridged plug-in's own state may have changed (edits in its
                // editor, a preset it loaded itself, an AU editor closing).
                // Routed by the exact instance: it counts only for an insert of
                // this session that the bridge still has loaded, so a late
                // report about a closed project's plug-in changes nothing. The
                // stored blob is left alone; the next save captures the state.
                ClientEvent::Host(HostEvent::PluginStateTouched { plugin_instance_id }) => {
                    let loaded = runtime
                        .lock()
                        .is_ok_and(|runtime| runtime.is_loaded(&plugin_instance_id));
                    let owned = !self
                        .timeline
                        .read(cx)
                        .state
                        .insert_owner_ids_containing(&plugin_instance_id)
                        .is_empty();
                    ped_log!(
                        "plugin state touched instance={plugin_instance_id} loaded={loaded} \
                         owned={owned}"
                    );
                    if loaded && owned {
                        self.note_plugin_state_edited(cx);
                    }
                }
                // Space was pressed while a plug-in editor owned keyboard focus.
                // Keyboard input goes to the thread owning the focused window,
                // so the main window never saw it. Run the same command the
                // arrangement's spacebar and the chrome Play button run — the
                // host reports the key, this process decides what it means.
                ClientEvent::Host(HostEvent::TransportToggleRequested { source, time_ms }) => {
                    // Handed to the process-wide transport-key router, not run
                    // here. Several independent claim paths can see one physical
                    // press — this process's message hook over an embedded view,
                    // the editor shells' window procedures, and the host's own
                    // filters — and each of them running play/pause for what it
                    // collected made a single Space play and immediately stop.
                    //
                    // `time_ms` is the press's Win32 message time, which is the
                    // same tick count in the host process and in this one, so
                    // the router can tell a second report of one press from a
                    // second press.
                    if transport_key::claim(TransportKeySource::PluginHost, time_ms) {
                        eprintln!(
                            "[Keyboard] plug-in host reported the transport key source={source} time={}",
                            time_ms
                                .map(|t| t.to_string())
                                .unwrap_or_else(|| "-".to_string())
                        );
                    }
                    changed = true;
                }
                _ => {}
            }
        }
        // Forward editor-targeted events to the owning editor window(s).
        for (instance, event) in editor_routes {
            self.dispatch_editor_event(&instance, event, cx);
        }
        if disconnect_all {
            self.broadcast_editor_disconnect(cx);
        }
        if changed {
            cx.notify();
        }
    }

    /// Route a single editor-targeted host event to the native main-owned editor
    /// shell that owns `plugin_instance_id` (IPC uses `plugin_instance_id`,
    /// Part 4). Editor host events only ever target bridge sessions — the legacy
    /// in-process editor path never produces them.
    fn dispatch_editor_event(
        &mut self,
        plugin_instance_id: &str,
        event: SpherePluginHost::plugin_host_client::ClientEvent,
        cx: &mut Context<Self>,
    ) {
        use SpherePluginHost::ipc::HostEvent;
        use SpherePluginHost::plugin_host_client::ClientEvent;

        // The GPUI editor window first: it is where a bridged insert's editor
        // lives now. Its own drive loop never drains the shared queue -- this
        // poll is the single drain -- so an event that is not handed over here
        // is lost and the window sits on "Loading" forever.
        // Matched on what the window actually hosts, not on the key it was
        // filed under: a channel's window keeps the key of whichever insert
        // opened it first, and its other tabs would never be found by that.
        let gpui_target = self
            .plugin_editors
            .open
            .iter()
            .map(|(key, handle)| (key.clone(), *handle))
            .find(|(_, handle)| {
                handle
                    .read_with(cx, |editor, _cx| editor.hosts_insert(plugin_instance_id))
                    .unwrap_or(false)
            });
        if let Some((key, handle)) = gpui_target {
            // An `EditorClosed` only means *this* window when this window has an
            // editor to lose. One arriving while it is still opening belongs to
            // whatever was torn down to make room for it.
            let attached = handle
                .update(cx, |editor, _window, _cx| editor.is_attached())
                .unwrap_or(false);
            let closed = attached
                && matches!(
                    event,
                    ClientEvent::Host(HostEvent::EditorClosed { .. }) | ClientEvent::Disconnected
                );
            let delivered = handle
                .update(cx, |editor, window, cx| {
                    editor.ingest_host_event(event.clone(), window, cx);
                })
                .is_ok();
            if delivered {
                if closed {
                    // The host let the editor go; drop the window with it rather
                    // than leaving an empty frame behind.
                    let _ = handle.update(cx, |_editor, window, _cx| window.remove_window());
                    self.plugin_editors.open.remove(&key);
                }
                return;
            }
            // Stale handle -- the window is gone. Fall through so the legacy
            // shell still gets a chance at the event.
            self.plugin_editors.open.remove(&key);
        }

        // Clone the shared-runtime Arc up front so we can send ResizeEditor while
        // holding a `&mut` borrow of the matched session.
        let runtime = self.plugin_editors.bridge_runtime.as_ref().cloned();
        let Some((_, session)) = self
            .plugin_editors
            .bridge
            .iter_mut()
            .find(|((_, id), _)| id == plugin_instance_id)
        else {
            eprintln!(
                "[plugin-bridge] editor event for instance={plugin_instance_id} dropped (no native editor shell)"
            );
            return;
        };

        // When the host reports the editor window is gone (e.g. the user closed
        // the host-owned window directly), drop the session after the match so a
        // later Open starts fresh instead of focusing a dead session.
        let mut remove_session_key: Option<(String, String)> = None;

        match event {
            ClientEvent::Host(HostEvent::EditorAttached {
                result,
                preferred_width,
                preferred_height,
                resizable,
                host_hwnd,
                ..
            }) => {
                let was = session.state.clone();
                transition_bridge_editor_state(session, BridgeEditorState::Attached, "host_event");
                session.host_hwnd = host_hwnd;
                session.attached_at = Some(Instant::now());
                session.paint_count_at_attach = session.shell.paint_stats().content_paint_count;
                session.first_paint_logged = false;
                ped_log!(
                    "Open Result instance={plugin_instance_id} hwnd=0x{host_hwnd:x} \
                     view_size={preferred_width}x{preferred_height} resizable={resizable} \
                     mode=external_main_owned state=Open total_ms={}",
                    session.requested_at.elapsed().as_millis()
                );
                session.shell.mark_attached();
                // VST3 resize contract (IPlugView::canResize): fixed-size
                // editors lock the wrapper so dragging can never open blank
                // area around the plugin view.
                session.shell.set_resizable(resizable);
                session.shell.focus();
                session.shell.pump_messages();
                let _ = self.timeline.update(cx, |timeline, _cx| {
                    let track_ids: Vec<String> = timeline
                        .state
                        .tracks
                        .iter()
                        .filter(|track| {
                            track
                                .inserts
                                .iter()
                                .any(|slot| slot.id == plugin_instance_id)
                        })
                        .map(|track| track.id.clone())
                        .collect();
                    let host_pid = runtime
                        .as_ref()
                        .and_then(|rt| rt.lock().ok())
                        .and_then(|r| r.host_pid());
                    track_ids.into_iter().any(|track_id| {
                        timeline.state.set_insert_runtime(
                            &track_id,
                            plugin_instance_id,
                            PluginRuntimeBackend::ExternalBridge,
                            PluginRuntimeState::EditorOpen,
                            host_pid,
                        )
                    })
                });
                eprintln!(
                    "[PluginHost] editor opened id={plugin_instance_id} hwnd=0x{host_hwnd:x}"
                );
                if was != BridgeEditorState::Attached {
                    eprintln!(
                        "[plugin-editor-window] plugin_instance_id={plugin_instance_id} editor_window_id=0x{:x}",
                        session.shell.top_hwnd()
                    );
                    eprintln!("[plugin-editor-window] state {was:?} -> Attached");
                    eprintln!("[plugin-editor-window] loading_overlay_visible=false");
                    eprintln!(
                        "[plugin-editor-window] native_content_region_reserved=true gpui_paints_over_content=false"
                    );
                }
                eprintln!(
                    "[plugin-view][host] EditorAttached instance={plugin_instance_id} \
                     attached_result={result} preferred={preferred_width}x{preferred_height} host_hwnd=0x{host_hwnd:x}"
                );
                if !session.preferred_applied {
                    apply_bridge_preferred(
                        session,
                        runtime.as_ref(),
                        preferred_width,
                        preferred_height,
                    );
                }
                let plugin_path = runtime
                    .as_ref()
                    .and_then(|rt| rt.lock().ok())
                    .and_then(|r| r.loaded_descriptor(plugin_instance_id))
                    .map(|p| p.descriptor.plugin_path)
                    .unwrap_or_else(|| "<unknown>".to_string());
                session.shell.apply_content_layout();
                if host_hwnd != 0 {
                    session.shell.log_black_gap_check(host_hwnd);
                }
                log_bridge_gpu_diagnostics(session, plugin_instance_id, &plugin_path);
                log_bridge_paint_stats(session);
            }
            ClientEvent::Host(HostEvent::EditorContentResize { width, height, .. }) => {
                eprintln!(
                    "[plugin-bridge] event EditorContentResize instance={plugin_instance_id} width={width} height={height}"
                );
                // Host-owned: the host window resizes itself (user drag /
                // resizeView). The main app owns no window and must not echo a
                // ResizeEditor back, or it would fight the host's own geometry.
                if session.shell.is_host_owned_proxy() {
                    // nothing to mirror
                } else if width > 0 && height > 0 {
                    resize_shell_before_attach(session, width, height);
                    if bridge_editor_is_open(&session.state) {
                        if let Some(rt) = runtime.as_ref() {
                            if let Ok(mut r) = rt.lock() {
                                let (cw, ch) = session.shell.content_size();
                                r.resize_editor(
                                    session.instance_id.clone(),
                                    cw as u32,
                                    ch as u32,
                                    bridge_editor_dpi(session),
                                );
                            }
                        }
                    }
                }
            }
            ClientEvent::Host(HostEvent::EditorPreferredSize { width, height, .. }) => {
                eprintln!(
                    "[plugin-bridge] event EditorPreferredSize instance={plugin_instance_id} width={width} height={height}"
                );
                if matches!(
                    session.state,
                    BridgeEditorState::ParentWindowCreated | BridgeEditorState::ViewCreated
                ) {
                    if width > 0 && height > 0 {
                        resize_shell_before_attach(session, width, height);
                    } else {
                        eprintln!(
                            "[plugin-editor-window] preferred_size_missing using_shell_default instance={plugin_instance_id}"
                        );
                    }
                    let content_hwnd = session.shell.content_hwnd();
                    let (cw, ch) = session.shell.content_size();
                    transition_bridge_editor_state(
                        session,
                        BridgeEditorState::Sized,
                        "preferred_size",
                    );
                    if let Some(rt) = runtime.as_ref() {
                        if let Ok(mut r) = rt.lock() {
                            let confirm = r.confirm_editor_content_ready(
                                session.instance_id.clone(),
                                content_hwnd,
                                cw as u32,
                                ch as u32,
                                bridge_editor_dpi(session),
                            );
                            if let Err(e) = confirm {
                                eprintln!(
                                    "[plugin-bridge] ConfirmEditorContentReady FAILED instance={plugin_instance_id} err={e}"
                                );
                                ped_log!(
                                    "Open Failed instance={plugin_instance_id} reason=ipc_error detail={e}"
                                );
                                session
                                    .shell
                                    .set_status(&format!("Editor failed: {e}"), true);
                                transition_bridge_editor_state(
                                    session,
                                    BridgeEditorState::Failed(e.to_string()),
                                    "confirm_content_ready_failed",
                                );
                            } else {
                                ped_log!(
                                    "state Preparing -> AwaitingAttach instance={plugin_instance_id} content={cw}x{ch}"
                                );
                                transition_bridge_editor_state(
                                    session,
                                    BridgeEditorState::AwaitingAttach,
                                    "content_ready_confirmed",
                                );
                            }
                        }
                    }
                } else if !bridge_editor_is_open(&session.state) {
                    apply_bridge_preferred(session, runtime.as_ref(), width, height);
                }
            }
            ClientEvent::Host(HostEvent::EditorAttachFailed { error, .. }) => {
                ped_log!(
                    "Open Failed instance={plugin_instance_id} reason=attach_failed detail={error} total_ms={}",
                    session.requested_at.elapsed().as_millis()
                );
                eprintln!(
                    "[plugin-view][host] EditorAttachFailed instance={plugin_instance_id} error={error}"
                );
                session
                    .shell
                    .set_status(&format!("Editor failed: {error}"), true);
                let timed_out = error.to_ascii_lowercase().contains("timed out")
                    || error.to_ascii_lowercase().contains("timeout");
                transition_bridge_editor_state(
                    session,
                    if timed_out {
                        BridgeEditorState::TimedOut(error)
                    } else {
                        BridgeEditorState::Failed(error)
                    },
                    "host_attach_failed",
                );
            }
            ClientEvent::Host(HostEvent::EditorUnresponsive { gap_ms, .. }) => {
                // Host UI thread pump stalled (freeze watchdog, spec item 10).
                // The wrapper window + close button live in THIS process, so the
                // user can always close the editor; surface the stall and keep
                // the session alive — the host usually recovers.
                eprintln!(
                    "[plugin-view][host] EditorUnresponsive instance={plugin_instance_id} gap_ms={gap_ms}"
                );
                if !bridge_editor_is_open(&session.state) {
                    session.shell.set_status(
                        "Plugin editor not responding — you can close this window.",
                        true,
                    );
                }
            }
            ClientEvent::Host(HostEvent::EditorClosed { .. }) => {
                eprintln!("[plugin-view][host] EditorClosed instance={plugin_instance_id}");
                let host_pid = runtime
                    .as_ref()
                    .and_then(|rt| rt.lock().ok())
                    .and_then(|r| r.host_pid());
                self.timeline.update(cx, |timeline, _cx| {
                    let track_ids: Vec<String> = timeline
                        .state
                        .tracks
                        .iter()
                        .filter(|track| {
                            track
                                .inserts
                                .iter()
                                .any(|slot| slot.id == plugin_instance_id)
                        })
                        .map(|track| track.id.clone())
                        .collect();
                    for track_id in track_ids {
                        timeline.state.set_insert_runtime(
                            &track_id,
                            plugin_instance_id,
                            PluginRuntimeBackend::ExternalBridge,
                            PluginRuntimeState::EditorClosed,
                            host_pid,
                        );
                    }
                });
                eprintln!(
                    "[PluginHost] editor closed id={plugin_instance_id} instance_still_active=true"
                );
                remove_session_key = Some((session.track_id.clone(), session.instance_id.clone()));
            }
            _ => {}
        }
        // `session` borrow has ended; safe to mutate the session map.
        if let Some(key) = remove_session_key {
            self.plugin_editors.bridge.remove(&key);
            eprintln!(
                "[plugin-editor-window] bridge session dropped after EditorClosed instance={plugin_instance_id} (reopen will start fresh)"
            );
        }
        cx.notify();
    }

    /// Host process disconnected (crash/exit): mark every open native editor
    /// session failed so none waits forever (spec Part 9 — surface, no fallback).
    fn broadcast_editor_disconnect(&mut self, cx: &mut Context<Self>) {
        if self.plugin_editors.bridge.is_empty() {
            return;
        }
        for session in self.plugin_editors.bridge.values_mut() {
            session
                .shell
                .set_status("Plugin host disconnected (crashed or exited).", true);
            transition_bridge_editor_state(
                session,
                BridgeEditorState::Failed(
                    "Plugin host process disconnected (crashed or exited).".to_string(),
                ),
                "host_disconnected",
            );
        }
        cx.notify();
    }

    /// Open a native main-owned editor shell for a bridged insert (spec Part 7).
    /// Creates a real Win32 top-level window + content HWND and asks the host to
    /// attach the VST3 view into it. No GPUI surface is composited over the
    /// content, so the plugin actually paints. Re-open focuses the existing shell.
    pub(super) fn open_bridge_editor(
        &mut self,
        track_id: &str,
        instance_id: &str,
        display_name: String,
        owner_hwnd: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let request_started = Instant::now();
        let host_owned = bridge_editor_host_owned();
        eprintln!(
            "[EDITOR OPEN START]\nplugin_instance_id={instance_id}\ntrack_id={track_id}\nstate=Opening"
        );
        eprintln!(
            "[plugin-editor-window] ownership={} forced=true",
            if host_owned {
                "host_owned"
            } else {
                "main_owned"
            }
        );
        // Spec freeze guard: opening the editor must never block the GPUI main
        // thread. In host-owned mode this is structural — the main app neither
        // creates a window nor waits for the host; it sends one non-blocking IPC
        // frame and returns. A debug_assert at the end of this fn proves it.
        eprintln!(
            "[MAIN_UI_FREEZE_GUARD]\nthread_id={:?}\nis_main_ui_thread=true\noperation=open_editor\nblocking_call_detected=false\nplugin_instance_id={instance_id}\nhost_owned={host_owned}\npanic_if_blocking_in_debug=true",
            std::thread::current().id()
        );
        self.log_editor_engine_state("open requested while", track_id, instance_id);
        crate::forensic_trace::log_trace_plugin(track_id, instance_id);
        let key = (track_id.to_string(), instance_id.to_string());

        // Re-open semantics (spec A6). An existing session is one of:
        //   * Attached / in-flight  -> focus the live (or loading) shell; never
        //     spawn a duplicate window for the same plugin instance.
        //   * Failed (incl. timed out) -> drop it and fall through to a fresh
        //     open. This is the fix for "cannot open again": the old code
        //     focused ANY existing session, so a stalled/failed open
        //     permanently blocked reopen.
        let existing = self
            .plugin_editors
            .bridge
            .get(&key)
            .map(|s| (s.state.clone(), s.requested_at));
        let mut loading_session = None;
        if let Some((state, requested_at)) = existing {
            match editor_reopen_action(&state) {
                EditorReopenAction::DropAndRetry => {
                    ped_log!(
                        "Open Request track={track_id} slot={instance_id} prior={state:?} -> retry (dropping stale session)"
                    );
                    self.close_bridge_editor(cx, track_id, instance_id);
                    // fall through to a fresh open below
                }
                EditorReopenAction::ReuseLoadingShell => {
                    ped_log!(
                        "Open Request track={track_id} slot={instance_id} state=Loading -> attach existing shell (loading_ms={})",
                        requested_at.elapsed().as_millis()
                    );
                    loading_session = self.plugin_editors.bridge.remove(&key);
                }
                EditorReopenAction::FocusExisting => {
                    if let Some(session) = self.plugin_editors.bridge.get(&key) {
                        session.shell.focus();
                    }
                    ped_log!(
                        "Open Request track={track_id} slot={instance_id} state={state:?} -> focus existing (in_flight_ms={})",
                        requested_at.elapsed().as_millis()
                    );
                    eprintln!(
                        "[plugin-editor-window] existing native editor focus instance={instance_id}"
                    );
                    return;
                }
            }
        }
        ped_log!(
            "Open Request track={track_id} slot={instance_id} state=Closed plugin={display_name} command=PrepareEditorView"
        );
        let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref().cloned() else {
            eprintln!(
                "[plugin-runtime] external bridge mandatory but no runtime for editor instance={instance_id}"
            );
            return;
        };

        let defaults = shell_defaults();
        let content_w = defaults.default_content_width;
        let content_h = defaults.default_content_height;
        let shell = if host_owned {
            // Host-owned: the temporary loading shell is main-owned and visible
            // only while the plugin loads. Once the host can open the real
            // editor, drop that shell and return to a proxy session so the
            // plugin view remains fully owned by the host process.
            drop(loading_session);
            NativeEditorShell::host_owned_proxy(&display_name)
        } else if let Some(session) = loading_session {
            session
                .shell
                .set_status("Attaching plugin editor...", false);
            session.shell
        } else {
            let Some(shell) =
                NativeEditorShell::create(&display_name, content_w, content_h, owner_hwnd)
            else {
                eprintln!(
                    "[plugin-editor-window] native shell create FAILED instance={instance_id}"
                );
                return;
            };
            shell
        };
        let content_hwnd = shell.content_hwnd();
        let (cw, ch) = shell.content_size();
        eprintln!(
            "[EDITOR HWND]\nplugin_instance_id={instance_id}\nshell_hwnd=0x{:x}\ncontent_hwnd=0x{content_hwnd:x}\ncontent_size={cw}x{ch}\nowner=main_process",
            shell.top_hwnd()
        );
        eprintln!(
            "[EDITOR STATE TRANSITION]\nplugin_instance_id={instance_id}\nfrom=Opening\nto=ParentWindowCreated\nreason=content_hwnd_created\nelapsed_ms={}",
            request_started.elapsed().as_millis()
        );
        eprintln!(
            "[plugin-editor-crossprocess] shell_pid={} content_hwnd=0x{content_hwnd:x} owner=main_process",
            std::process::id()
        );
        crate::components::gpu_editor_diagnostics::log_window_style_audit(
            shell.top_hwnd(),
            content_hwnd,
            0,
        );
        let open_result = if host_owned {
            // Ask the host to create+own+attach its own detached window. The
            // owner HWND is a *read-only* DPI/position reference (IsWindow /
            // GetWindowRect / GetDpiForWindow are non-blocking cross-process
            // queries) — never a parent, so no input-queue coupling. One
            // non-blocking IPC frame; the host replies EditorAttached async.
            //
            // On Linux the host opens a top-level GTK/X11 editor and ignores
            // parent_hwnd as a real parent. Allow owner_ref=0 when the GPUI
            // window has no X11 handle (pure Wayland) so IPC still reaches the
            // host; the GTK path reports a clear error if X11 is unavailable.
            let dpi = shell.shell_dpi();
            let parent = owner_hwnd.unwrap_or(0);
            eprintln!(
                "[plugin-bridge] sending OpenEditorWithParentHwnd (host-owned) instance={instance_id} owner_ref=0x{parent:x} size={content_w}x{content_h} dpi={dpi}"
            );
            runtime
                .lock()
                .map_err(|_| "bridge runtime lock poisoned".to_string())
                .and_then(|mut r| {
                    r.open_editor_with_parent(
                        instance_id.to_string(),
                        parent,
                        content_w as u32,
                        content_h as u32,
                        dpi,
                    )
                    .map_err(|e| e.to_string())
                })
        } else {
            eprintln!(
                "[plugin-bridge] sending PrepareEditorView instance={instance_id} shell_content=0x{content_hwnd:x} size={cw}x{ch}"
            );
            runtime
                .lock()
                .map_err(|_| "bridge runtime lock poisoned".to_string())
                .and_then(|mut r| {
                    r.prepare_editor_view(instance_id.to_string())
                        .map_err(|e| e.to_string())
                })
        };
        match open_result {
            Ok(()) => {
                let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                eprintln!(
                    "[editor-open] 04 sending_ipc_open_editor host_id={host_pid:?} insert_id={instance_id} host_owned={host_owned} size={content_w}x{content_h}"
                );
                self.timeline.update(cx, |timeline, _cx| {
                    timeline.state.set_insert_runtime(
                        track_id,
                        instance_id,
                        PluginRuntimeBackend::ExternalBridge,
                        PluginRuntimeState::EditorOpening,
                        host_pid,
                    );
                });
                eprintln!("[PluginHost] editor reopen requested id={instance_id}");
                ped_log!(
                    "Open dispatched track={track_id} slot={instance_id} state=Preparing request_to_ipc_ms={}",
                    request_started.elapsed().as_millis()
                );
                eprintln!(
                    "[EDITOR STATE TRANSITION]\nplugin_instance_id={instance_id}\nfrom=ParentWindowCreated\nto=ViewCreated\nreason=prepare_editor_view_sent\nelapsed_ms={}",
                    request_started.elapsed().as_millis()
                );
                self.plugin_editors.bridge.insert(
                    key,
                    BridgeEditorSession {
                        track_id: track_id.to_string(),
                        instance_id: instance_id.to_string(),
                        display_name,
                        shell,
                        state: BridgeEditorState::ViewCreated,
                        preferred_applied: false,
                        last_content: (cw, ch),
                        host_hwnd: 0,
                        requested_at: request_started,
                        attached_at: None,
                        paint_count_at_attach: 0,
                        first_paint_logged: false,
                    },
                );
                if let Some(engine) = self.audio_bridge.engine.as_ref() {
                    let _ = engine.set_bridge_editor_active(track_id.to_string(), true);
                }
                self.log_editor_engine_state(
                    "open complete engine_state_after=",
                    track_id,
                    instance_id,
                );
                cx.notify();
            }
            Err(e) => {
                ped_log!(
                    "Open Failed track={track_id} slot={instance_id} reason=ipc_error detail={e}"
                );
                eprintln!(
                    "[plugin-editor-window] open bridge editor FAILED instance={instance_id} err={e}"
                );
                eprintln!("[editor-open] FAILED stage=ipc_send instance={instance_id} reason={e}");
                // Never fail silently: surface the IPC failure on the editor shell
                // so the user sees the failing stage instead of a dead window.
                shell.set_status(
                    &format!("Plugin editor open failed\nStage: ipc_send\n{e}"),
                    true,
                );
            }
        }
        // Spec freeze guard (debug): the open path does proxy/window creation and
        // exactly one non-blocking IPC frame, then returns to the GPUI event
        // loop. If it ever took ~1s the main thread was blocked — assert in debug
        // so a regression that reintroduces a synchronous wait is caught at once.
        let open_elapsed = request_started.elapsed();
        debug_assert!(
            open_elapsed < Duration::from_secs(1),
            "[MAIN_UI_FREEZE_GUARD] open_bridge_editor blocked the GPUI main thread for {}ms (instance={instance_id}, host_owned={host_owned})",
            open_elapsed.as_millis()
        );
    }

    pub(super) fn open_bridge_loading_editor(
        &mut self,
        track_id: &str,
        instance_id: &str,
        display_name: String,
        owner_hwnd: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        let key = (track_id.to_string(), instance_id.to_string());
        if let Some(session) = self.plugin_editors.bridge.get(&key) {
            if !bridge_editor_is_terminal(&session.state) {
                session.shell.focus();
                session
                    .shell
                    .set_status(&loading_plugin_status(&display_name), false);
                return;
            }
        }
        if matches!(
            self.plugin_editors
                .bridge
                .get(&key)
                .map(|session| &session.state),
            Some(BridgeEditorState::Failed(_)) | Some(BridgeEditorState::TimedOut(_))
        ) {
            self.close_bridge_editor(cx, track_id, instance_id);
        }

        let defaults = shell_defaults();
        let shell = match NativeEditorShell::create(
            &display_name,
            defaults.default_content_width,
            defaults.default_content_height,
            owner_hwnd,
        ) {
            Some(shell) => shell,
            None if bridge_editor_host_owned() => {
                eprintln!(
                    "[plugin-editor-window] native loading shell create FAILED instance={instance_id}; falling back to host-owned proxy"
                );
                NativeEditorShell::host_owned_proxy(&display_name)
            }
            None => {
                eprintln!(
                    "[plugin-editor-window] native loading shell create FAILED instance={instance_id}"
                );
                return;
            }
        };
        shell.set_status(&loading_plugin_status(&display_name), false);
        let (cw, ch) = shell.content_size();
        eprintln!("[plugin-editor-window] loading shell visible instance={instance_id}");
        self.plugin_editors.bridge.insert(
            key,
            BridgeEditorSession {
                track_id: track_id.to_string(),
                instance_id: instance_id.to_string(),
                display_name,
                shell,
                state: BridgeEditorState::Loading,
                preferred_applied: false,
                last_content: (cw, ch),
                host_hwnd: 0,
                requested_at: Instant::now(),
                attached_at: None,
                paint_count_at_attach: 0,
                first_paint_logged: false,
            },
        );
        cx.notify();
    }

    fn open_loading_editor_for_bound_insert(
        &mut self,
        track_id: &str,
        slot_id: &str,
        display_name: &str,
        owner_hwnd: Option<u64>,
        cx: &mut Context<Self>,
    ) {
        if !super::plugin_bridge_runtime::bridge_enabled() {
            return;
        }
        let _ = self.timeline.update(cx, |timeline, _cx| {
            timeline
                .state
                .set_insert_pending_editor_open(track_id, slot_id, true)
        });
        self.open_bridge_loading_editor(
            track_id,
            slot_id,
            display_name.to_string(),
            owner_hwnd,
            cx,
        );
    }

    /// Per-tick driver for native editor shells: honor OS close requests and
    /// forward window resizes to the host as `ResizeEditor` (spec Part 4/8). The
    /// content child is resized synchronously in the shell `WndProc`; this only
    /// pushes the matching `onSize` to the plugin.
    fn log_editor_engine_state(&self, phase: &str, track_id: &str, instance_id: &str) {
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            eprintln!(
                "[PluginEditor] {phase} engine_state_before=unknown transport_playing=unknown instance={instance_id}"
            );
            return;
        };
        let stats = engine.stats();
        let engine_state = if stats.transport_playing {
            "Running"
        } else {
            "Paused"
        };
        eprintln!(
            "[PluginEditor] {phase} engine_state={engine_state} transport_playing={} track={track_id} instance={instance_id}",
            stats.transport_playing
        );
    }

    pub(super) fn drive_bridge_editors(&mut self, cx: &mut Context<Self>) {
        if self.plugin_editors.bridge.is_empty() {
            return;
        }
        let runtime = self.plugin_editors.bridge_runtime.as_ref().cloned();
        let mut to_close: Vec<(String, String)> = Vec::new();
        let mut changed = false;
        for (key, session) in self.plugin_editors.bridge.iter_mut() {
            session.shell.pump_messages();
            // Open watchdog (spec A6): a session still loading past the deadline
            // is marked TimedOut so a subsequent Open click retries instead of
            // re-focusing a dead loading shell. Leaves the window up with a
            // status message so the user can also just close it.
            if matches!(
                session.state,
                BridgeEditorState::ParentWindowCreated
                    | BridgeEditorState::ViewCreated
                    | BridgeEditorState::Sized
                    | BridgeEditorState::AwaitingAttach
            ) && session.requested_at.elapsed() >= editor_open_timeout()
            {
                let open_timeout = editor_open_timeout();
                ped_log!(
                    "Open Failed instance={} reason=timeout state={:?} elapsed_s={}",
                    session.instance_id,
                    session.state,
                    session.requested_at.elapsed().as_secs()
                );
                eprintln!(
                    "[EDITOR HANG WATCHDOG]\nplugin_instance_id={}\nstage={}\nelapsed_ms={}\ntimeout_ms={}\nui_thread_responsive=true\nhost_process_alive=true",
                    session.instance_id,
                    bridge_editor_state_name(&session.state),
                    session.requested_at.elapsed().as_millis(),
                    open_timeout.as_millis()
                );
                session
                    .shell
                    .set_status("Plugin editor timed out. Close and open it again.", true);
                transition_bridge_editor_state(
                    session,
                    BridgeEditorState::TimedOut("Editor open timed out".to_string()),
                    "open_watchdog",
                );
                changed = true;
                continue;
            }
            if session.shell.is_host_owned_proxy() {
                // Host-owned: the editor window (and its painting) live in the
                // host process — the main app cannot observe content paints, so
                // the first-paint watchdog does not apply. EditorAttached is the
                // authoritative "open" signal; promote it straight to Ready.
                if session.state == BridgeEditorState::Attached {
                    transition_bridge_editor_state(
                        session,
                        BridgeEditorState::Ready,
                        "host_owned_attached",
                    );
                    changed = true;
                }
            } else if matches!(
                session.state,
                BridgeEditorState::Attached | BridgeEditorState::Visible
            ) {
                let stats = session.shell.paint_stats();
                if !session.first_paint_logged
                    && stats.content_paint_count > session.paint_count_at_attach
                {
                    eprintln!(
                        "[EDITOR FIRST PAINT]\nplugin_instance_id={}\ncontent_paint_count={}\nelapsed_after_attach_ms={}\ntotal_elapsed_ms={}",
                        session.instance_id,
                        stats.content_paint_count,
                        session
                            .attached_at
                            .map(|t| t.elapsed().as_millis())
                            .unwrap_or_default(),
                        session.requested_at.elapsed().as_millis()
                    );
                    session.first_paint_logged = true;
                    transition_bridge_editor_state(
                        session,
                        BridgeEditorState::Visible,
                        "content_paint_after_attach",
                    );
                    transition_bridge_editor_state(
                        session,
                        BridgeEditorState::Ready,
                        "first_paint_observed",
                    );
                    changed = true;
                } else if !session.first_paint_logged
                    && session.attached_at.is_some_and(|attached_at| {
                        attached_at.elapsed() >= EDITOR_FIRST_PAINT_TIMEOUT
                    })
                {
                    eprintln!(
                        "[EDITOR HANG WATCHDOG]\nplugin_instance_id={}\nstage=first_paint\nelapsed_ms={}\ntimeout_ms={}\nui_thread_responsive=true\nhost_process_alive=true",
                        session.instance_id,
                        session
                            .attached_at
                            .map(|t| t.elapsed().as_millis())
                            .unwrap_or_default(),
                        EDITOR_FIRST_PAINT_TIMEOUT.as_millis()
                    );
                    if let Some(rt) = runtime.as_ref() {
                        if let Ok(mut r) = rt.lock() {
                            r.close_editor(session.instance_id.clone());
                        }
                    }
                    session.host_hwnd = 0;
                    session.shell.set_status(
                        "Plugin editor attached but did not paint. Close and open it again.",
                        true,
                    );
                    transition_bridge_editor_state(
                        session,
                        BridgeEditorState::TimedOut("Editor first paint timed out".to_string()),
                        "first_paint_watchdog",
                    );
                    changed = true;
                    continue;
                }
            }
            let poll = session.shell.poll();
            if poll.close_requested {
                eprintln!(
                    "[plugin-editor-window] user close requested instance={}",
                    session.instance_id
                );
                to_close.push(key.clone());
                continue;
            }
            if let Some((w, h)) = poll.resized {
                if w > 0 && h > 0 && (w, h) != session.last_content {
                    session.last_content = (w, h);
                    if bridge_editor_is_open(&session.state) {
                        if let Some(rt) = runtime.as_ref() {
                            if let Ok(mut r) = rt.lock() {
                                r.resize_editor(
                                    session.instance_id.clone(),
                                    w as u32,
                                    h as u32,
                                    bridge_editor_dpi(session),
                                );
                            }
                        }
                        if session.host_hwnd != 0 {
                            session.shell.log_black_gap_check(session.host_hwnd);
                        }
                    }
                    // No ensure_visible_zorder here: the shell WM_SIZE path
                    // already repositions the content child, and forcing a
                    // host-subtree repaint per resize event stalls both UI
                    // threads (cross-process synchronous edge).
                    eprintln!(
                        "[plugin-bridge] ResizeEditor instance={} width={w} height={h}",
                        session.instance_id
                    );
                    changed = true;
                }
            }
        }
        for key in to_close {
            self.close_bridge_editor(cx, &key.0, &key.1);
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Close a native editor session: send `CloseEditor` to the host (view
    /// `removed()`), then drop the session so the shell window is destroyed.
    /// Only called on genuine close (user / replace / track-delete / shutdown).
    /// Drops a loading shell without telling the host anything.
    ///
    /// The plug-in's editor is not being closed — it is moving into the GPUI
    /// window that just opened — so the host must not be sent `CloseEditor`.
    fn drop_bridge_loading_shell(&mut self, track_id: &str, instance_id: &str) {
        let key = (track_id.to_string(), instance_id.to_string());
        if self.plugin_editors.bridge.remove(&key).is_some() {
            eprintln!(
                "[plugin-editor-window] loading shell dropped instance={instance_id}                  (editor moved into its own window)"
            );
        }
    }

    pub(super) fn close_bridge_editor(
        &mut self,
        cx: &mut Context<Self>,
        track_id: &str,
        instance_id: &str,
    ) {
        let key = (track_id.to_string(), instance_id.to_string());
        eprintln!("[PluginEditor] close requested plugin_id={instance_id}");
        self.log_editor_engine_state("close engine_state_before=", track_id, instance_id);
        if let Some(session) = self.plugin_editors.bridge.remove(&key) {
            if let Some(engine) = self.audio_bridge.engine.as_ref() {
                let _ = engine.set_bridge_editor_active(track_id.to_string(), false);
            }
            if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
                if let Ok(mut r) = runtime.lock() {
                    r.close_editor(session.instance_id.clone());
                }
            }
            eprintln!("[PluginEditor] detached editor only plugin_id={instance_id}");
            eprintln!("[PluginRuntime] instance remains alive plugin_id={instance_id}");
            eprintln!("[AudioGraph] node remains active plugin_id={instance_id}");
            eprintln!("[VSTi] midi route alive plugin_id={instance_id}");
            eprintln!("[VSTi] process active after editor close plugin_id={instance_id}");
            eprintln!(
                "[plugin-editor-window] close native editor instance={instance_id} (CloseEditor sent, shell destroyed, DSP remains active)"
            );
            self.log_editor_engine_state("close engine_state_after=", track_id, instance_id);
            let host_pid = self
                .plugin_editors
                .bridge_runtime
                .as_ref()
                .and_then(|rt| rt.lock().ok())
                .and_then(|r| r.host_pid());
            let _ = self.timeline.update(cx, |timeline, cx| {
                timeline.state.set_insert_runtime(
                    track_id,
                    instance_id,
                    PluginRuntimeBackend::ExternalBridge,
                    PluginRuntimeState::EditorClosed,
                    host_pid,
                );
                cx.notify();
            });
        }
    }

    /// Lazily populated cache of registered audio plugins. First call
    /// runs `PluginRegistry::scan(None)` synchronously — the SQLite
    /// cache backing the registry makes subsequent scans fast. The UI
    /// thread blocks here on purpose; the audio thread is untouched.
    /// `None` return = registry has zero insert-capable plugins.
    /// Open the GPUI-hosted native editor window for an insert slot (Phase 4).
    /// GPUI owns a borderless shell; the C++ backend embeds the VST3 IPlugView
    /// in a native child region under it. If already open, this is a no-op (the
    /// window stays up). UI thread only; bad plugin → the editor window shows a
    /// fallback panel, never a crash.
    /// Opens the GPUI editor window for one bridged insert.
    ///
    /// Called from a spawned task, never from inside a frame — see the note at
    /// its only call site.
    fn open_bridged_editor_window(
        &mut self,
        owner_bounds: gpui::Bounds<gpui::Pixels>,
        key: (String, String),
        track_id: String,
        insert_id: String,
        display_name: String,
        cx: &mut Context<Self>,
    ) {
        // Nothing is guaranteed to still be true a turn later: the insert may
        // have been removed, or a second click may have opened the window
        // already.
        if self.plugin_editors.open.contains_key(&key) {
            return;
        }
        // One window per channel. A second plug-in on the same channel becomes
        // another tab in the window that is already open rather than a window of
        // its own — the inserts are one chain, and the user is moving along it.
        {
            let tabs = self
                .plugin_editors
                .editor_tabs
                .entry(track_id.clone())
                .or_default();
            if !tabs.iter().any(|id| id == &insert_id) {
                tabs.push(insert_id.clone());
            }
        }
        if let Some(existing) = self.plugin_editor_window_for(&track_id) {
            let reused = existing
                .update(cx, |editor, window, cx| {
                    editor.activate_tab(&insert_id, &display_name, window, cx);
                    window.activate_window();
                })
                .is_ok();
            if reused {
                self.drop_bridge_loading_shell(&track_id, &insert_id);
                self.refresh_plugin_editor_chrome(cx);
                return;
            }
            // The window is gone -- closed from its own titlebar. Drop it and
            // the tabs it was showing, then open a fresh one below.
            self.plugin_editors
                .open
                .retain(|(track, _), _| track != &track_id);
            self.plugin_editors
                .editor_tabs
                .insert(track_id.clone(), vec![insert_id.clone()]);
        }
        let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref().cloned() else {
            eprintln!(
                "[plugin-runtime] external bridge mandatory but no runtime for editor                  instance={insert_id}"
            );
            return;
        };
        match crate::components::plugin_editor_window::open_plugin_editor_window(
            owner_bounds,
            track_id.clone(),
            insert_id.clone(),
            display_name,
            None,
            Some(runtime),
            false,
            cx,
        ) {
            Ok(handle) => {
                self.plugin_editors.open.insert(key, handle);
                // The loading shell put up while the plug-in was still being
                // loaded has been replaced by this window. Dropped locally, not
                // closed through the host: `close_bridge_editor` sends
                // CloseEditor, the host answers EditorClosed, and that answer --
                // arriving a moment after this window opened -- would be read as
                // "this editor was closed" and take the new window down with it.
                // That is the first open closing itself while the second, with
                // no shell to drop, works.
                self.drop_bridge_loading_shell(&track_id, &insert_id);
                self.refresh_plugin_editor_chrome(cx);
            }
            Err(err) => {
                eprintln!("[plugin-view] open FAILED track={track_id} slot={insert_id} err={err}");
            }
        }
    }

    pub(super) fn open_insert_editor(
        &mut self,
        track_id: &str,
        insert_index: usize,
        plugin_instance_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::components::timeline::timeline_state::{InsertLoadStatus, InsertPluginFormat};
        let debug = std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some();

        eprintln!(
            "[editor-open] 01 open_button_clicked track={track_id} slot={insert_index} instance={plugin_instance_id}"
        );

        let resolved = {
            let timeline = self.timeline.read(cx);
            let track_info = timeline
                .state
                .tracks
                .iter()
                .enumerate()
                .find(|(_, track)| track.id == track_id)
                .map(|(index, track)| (Some(index as u32), Some(track.name.clone())))
                .unwrap_or_else(|| {
                    if track_id == crate::components::timeline::timeline_state::MASTER_TRACK_ID {
                        (None, Some("Master".to_string()))
                    } else {
                        (None, None)
                    }
                });
            timeline
                .state
                .insert_slot_at(track_id, insert_index)
                .map(|slot| {
                    let insert_found = slot.id == plugin_instance_id;
                    (
                        track_info.0,
                        track_info.1,
                        insert_found,
                        slot.id.clone(),
                        slot.plugin_id.clone(),
                        slot.plugin_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                        slot.plugin_format,
                        slot.display_name.clone(),
                        slot.runtime_state.clone(),
                        slot.load_status.clone(),
                        slot.pending_open_editor,
                    )
                })
        };
        let Some((
            track_index,
            track_name,
            insert_found,
            resolved_plugin_instance_id,
            plugin_id,
            plugin_path,
            plugin_format,
            display_name,
            runtime_state,
            load_status,
            pending_editor_open,
        )) = resolved
        else {
            eprintln!(
                "[PluginEditor] open requested track={track_id} slot={insert_index} instance=<none>"
            );
            eprintln!("[PluginEditor] no runtime instance; cannot open");
            eprintln!(
                "[editor-open] FAILED stage=resolve_slot reason=slot_not_found track={track_id} slot={insert_index}"
            );
            return;
        };

        // Built-ins branch out here, before every VST3 gate below.
        //
        // This must stay ahead of the runtime/load-status gating: a built-in has
        // no VST3 runtime instance, no plugin-host process and no bridge
        // instance, so `runtime_state` is permanently `NotLoaded` and the gate
        // would `return` (queueing a pending open that never resolves). Its
        // editor is an embedded React UI served to CEF over `mikoplugin://` and
        // depends on none of that machinery.
        // NB: `is_builtin_ref`, not `is_builtin_id` — an insert slot's
        // `plugin_id` holds the registry *class id* (`rodharerist`), not the
        // catalog id (`builtin:rodharerist`).
        if let Some(id) = plugin_id.as_deref() {
            if SpherePluginHost::is_builtin_ref(id) {
                eprintln!(
                    "[editor-open] builtin route plugin={id} track={track_id} \
                     slot={insert_index} instance={resolved_plugin_instance_id}"
                );
                self.open_builtin_insert_editor(
                    track_id,
                    &resolved_plugin_instance_id,
                    id,
                    display_name,
                    window,
                    cx,
                );
                return;
            }
        }

        eprintln!(
            "[OpenEditor/UI] track_id={track_id} track_index={} track_name={} slot_id={resolved_plugin_instance_id} instance_id={resolved_plugin_instance_id} plugin={display_name}",
            track_index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            track_name.as_deref().unwrap_or("<unknown>"),
        );
        eprintln!(
            "[PluginEditor] open requested track={track_id} slot={insert_index} instance={resolved_plugin_instance_id}"
        );
        eprintln!(
            "[editor-open] 02 selected_track_resolved track_id={track_id} track_index={} track_name={}",
            track_index
                .map(|i| i.to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            track_name.as_deref().unwrap_or("<unknown>"),
        );
        eprintln!(
            "[PluginEditor] insert runtime_state={runtime_state:?} load_status={load_status:?}"
        );

        if !insert_found {
            eprintln!("[PluginEditor] no runtime instance; cannot open (insert id mismatch)");
            eprintln!(
                "[editor-open] FAILED stage=resolve_slot reason=insert_id_mismatch track={track_id} slot={insert_index} requested={plugin_instance_id} resolved={resolved_plugin_instance_id}"
            );
            return;
        }
        eprintln!(
            "[editor-open] 03 selected_insert_resolved insert_id={resolved_plugin_instance_id} plugin_name={display_name} format={plugin_format:?} runtime_state={runtime_state:?}"
        );
        if SpherePluginHost::plugin_host_client::vst3_editor_backend_disabled() {
            eprintln!(
                "[VST3Editor] backend=disabled action=skip_open instance={resolved_plugin_instance_id}"
            );
            return;
        }

        let insert_id = resolved_plugin_instance_id.as_str();
        let editor_session_key = (track_id.to_string(), insert_id.to_string());
        let (editor_state, editor_created, content_hwnd, content_size) = self
            .plugin_editors
            .bridge
            .get(&editor_session_key)
            .map(|session| {
                (
                    bridge_editor_state_name(&session.state).to_string(),
                    session.host_hwnd != 0,
                    session.shell.content_hwnd(),
                    session.shell.content_size(),
                )
            })
            .unwrap_or_else(|| ("Closed".to_string(), false, 0, (0, 0)));
        let bridge_loaded_for_open = if super::plugin_bridge_runtime::bridge_enabled() {
            self.plugin_editors
                .bridge_runtime
                .as_ref()
                .and_then(|runtime| runtime.lock().ok())
                .and_then(|runtime| runtime.loaded_descriptor(insert_id))
                .is_some()
        } else {
            false
        };
        let runtime_state_for_open = if bridge_loaded_for_open
            && matches!(runtime_state, PluginRuntimeState::EditorClosed)
        {
            eprintln!(
                "[PluginEditor] bridge instance already loaded; reconciling editor open state instance={insert_id} prior={runtime_state:?}"
            );
            let host_pid = self
                .plugin_editors
                .bridge_runtime
                .as_ref()
                .and_then(|runtime| runtime.lock().ok())
                .and_then(|runtime| runtime.host_pid());
            let _ = self.timeline.update(cx, |timeline, _cx| {
                timeline.state.set_insert_runtime(
                    track_id,
                    insert_id,
                    PluginRuntimeBackend::ExternalBridge,
                    PluginRuntimeState::Active,
                    host_pid,
                )
            });
            PluginRuntimeState::Active
        } else {
            runtime_state.clone()
        };
        let plugin_host_alive = self
            .plugin_editors
            .bridge_runtime
            .as_ref()
            .and_then(|runtime| runtime.lock().ok())
            .and_then(|runtime| runtime.host_pid())
            .is_some();
        let controller_known = plugin_id.is_some();
        let mut gate_allowed = true;
        let mut block_reason = "none";
        if matches!(runtime_state_for_open, PluginRuntimeState::Missing(_)) {
            gate_allowed = false;
            block_reason = "plugin_binary_missing";
        } else if matches!(runtime_state_for_open, PluginRuntimeState::Failed(_)) {
            gate_allowed = false;
            block_reason = "plugin_load_failed";
        } else if load_status != InsertLoadStatus::Ready {
            gate_allowed = false;
            block_reason = "load_status_not_ready";
        } else if matches!(runtime_state_for_open, PluginRuntimeState::Loading) {
            gate_allowed = false;
            block_reason = "plugin_load_state_loading";
        } else if matches!(
            runtime_state_for_open,
            PluginRuntimeState::NotLoaded | PluginRuntimeState::Unloaded
        ) {
            gate_allowed = false;
            block_reason = "runtime_instance_not_loaded";
        } else if super::plugin_bridge_runtime::bridge_enabled() && !plugin_host_alive {
            gate_allowed = false;
            block_reason = "plugin_host_not_alive";
        } else if super::plugin_bridge_runtime::bridge_enabled() && !bridge_loaded_for_open {
            gate_allowed = false;
            block_reason = "bridge_instance_missing";
        } else if !controller_known {
            gate_allowed = false;
            block_reason = "controller_unknown";
        }
        eprintln!(
            "[EDITOR OPEN GATE]\nplugin_instance_id={insert_id}\nruntime_state={runtime_state_for_open:?}\nload_status={load_status:?}\nplugin_load_state={load_status:?}\neditor_state={editor_state}\nbridge_instance_exists={bridge_loaded_for_open}\nplugin_host_alive={plugin_host_alive}\ncontroller_known={controller_known}\neditor_created={editor_created}\npending_editor_open={pending_editor_open}\ncontent_hwnd=0x{content_hwnd:x}\ncontent_size={}x{}\nallowed={gate_allowed}\nblock_reason={block_reason}",
            content_size.0, content_size.1
        );
        if !gate_allowed
            && !matches!(
                runtime_state_for_open,
                PluginRuntimeState::Missing(_)
                    | PluginRuntimeState::Failed(_)
                    | PluginRuntimeState::Loading
                    | PluginRuntimeState::NotLoaded
                    | PluginRuntimeState::Unloaded
            )
        {
            eprintln!(
                "[PluginEditor] editor open blocked; queueing editor open reason={block_reason}"
            );
            // Single pending request per instance: set the flag only (idempotent).
            // The plugin-ready transition (`on_bridge_plugin_host_ready` /
            // ProcessingPrepared) is the SOLE place that converts the flag into a
            // one-shot deferred open — never this blocked path, which `flush`
            // re-drives every frame and would otherwise re-queue infinitely.
            let queued = self.timeline.update(cx, |timeline, _cx| {
                timeline
                    .state
                    .set_insert_pending_editor_open(track_id, insert_id, true)
            });
            eprintln!(
                "[EDITOR_OPEN_GATE]\nplugin_instance_id={insert_id}\nblock_reason={block_reason}\npending_editor_open=true\naction=queue\nnewly_queued={queued}"
            );
            if super::plugin_bridge_runtime::bridge_enabled() {
                let owner_hwnd = studio_native_hwnd(window);
                self.open_bridge_loading_editor(
                    track_id,
                    insert_id,
                    display_name.clone(),
                    owner_hwnd,
                    cx,
                );
            }
            return;
        }

        match &runtime_state_for_open {
            PluginRuntimeState::Missing(reason) => {
                eprintln!("[PluginEditor] cannot open: plugin missing ({reason})");
                return;
            }
            PluginRuntimeState::Failed(reason) => {
                eprintln!("[PluginEditor] cannot open: plugin failed ({reason})");
                return;
            }
            PluginRuntimeState::Loading
            | PluginRuntimeState::NotLoaded
            | PluginRuntimeState::Unloaded => {
                eprintln!(
                    "[PluginEditor] editor open blocked; queueing editor open reason={block_reason}"
                );
                // Set the single pending flag only; do NOT push a deferred open
                // here (the ready transition owns that — see the gate block above).
                let queued = self.timeline.update(cx, |timeline, _cx| {
                    timeline
                        .state
                        .set_insert_pending_editor_open(track_id, insert_id, true)
                });
                eprintln!(
                    "[EDITOR_OPEN_GATE]\nplugin_instance_id={insert_id}\nblock_reason={block_reason}\npending_editor_open=true\naction=queue\nnewly_queued={queued}"
                );
                if super::plugin_bridge_runtime::bridge_enabled() {
                    let owner_hwnd = studio_native_hwnd(window);
                    self.open_bridge_loading_editor(
                        track_id,
                        insert_id,
                        display_name.clone(),
                        owner_hwnd,
                        cx,
                    );
                    if matches!(
                        runtime_state_for_open,
                        PluginRuntimeState::NotLoaded | PluginRuntimeState::Unloaded
                    ) {
                        let _ = self.load_bridge_insert_for_slot(track_id, insert_id, cx);
                    }
                }
                return;
            }
            _ => {}
        }
        let key = (track_id.to_string(), resolved_plugin_instance_id.clone());

        // One editor window per insert. If a live editor already exists for this
        // slot, focus/raise it instead of opening (or instantiating) a second
        // one. Only drop the handle when its window is actually gone.
        if let Some(handle) = self.plugin_editors.open.get(&key) {
            if handle
                .update(cx, |editor, window, cx| {
                    // Not `activate_window` directly: on a host-owned backend
                    // this window is not the one the user is asking for — the
                    // editor is in the plug-in host's own window, and raising
                    // an off-screen shell would look like nothing happened.
                    editor.focus_editor_surface(window, cx);
                })
                .is_ok()
            {
                if debug {
                    eprintln!(
                        "[plugin-view] existing editor found track={track_id} slot={insert_id} \
                         → focus (no new instance)"
                    );
                }
                return;
            }
            if debug {
                eprintln!(
                    "[plugin-view] stale editor handle track={track_id} slot={insert_id} → recreating"
                );
            }
            self.plugin_editors.open.remove(&key);
        }

        // NB: built-ins already returned near the top of this function. VST3
        // and Audio Unit editors are both host-owned native windows; an AU is
        // addressed by component id and therefore does not require a module
        // file path.
        let path = plugin_path.filter(|p| !p.trim().is_empty());
        let editable = match plugin_format {
            // Every module format has a native editor and a module file.
            Some(
                InsertPluginFormat::Vst3 | InsertPluginFormat::Vst2 | InsertPluginFormat::Clap,
            ) => path.is_some() && plugin_id.is_some(),
            Some(InsertPluginFormat::Au) => plugin_id.is_some(),
            _ => false,
        };
        if !editable {
            eprintln!(
                "[PluginEditor] cannot open: not editable fmt={plugin_format:?} path={path:?}"
            );
            return;
        }
        if SpherePluginHost::plugin_host_client::vst3_editor_backend_disabled() {
            eprintln!("[VST3Editor] backend=disabled action=skip_open instance={insert_id}");
            return;
        }

        if super::plugin_bridge_runtime::bridge_enabled() {
            if !bridge_loaded_for_open {
                eprintln!("[PluginEditor] no runtime instance; loading plugin");
                let owner_hwnd = studio_native_hwnd(window);
                self.open_bridge_loading_editor(
                    track_id,
                    insert_id,
                    display_name.clone(),
                    owner_hwnd,
                    cx,
                );
                if self.load_bridge_insert_for_slot(track_id, insert_id, cx) {
                    // Set the pending flag only; the ready transition pushes the
                    // one-shot deferred open once the plugin confirms loaded.
                    let _ = self.timeline.update(cx, |timeline, _cx| {
                        timeline
                            .state
                            .set_insert_pending_editor_open(track_id, insert_id, true);
                    });
                }
                return;
            }
            eprintln!("[PluginEditor] opening instance={insert_id}");
            if super::plugin_editor_chrome_ops::legacy_native_editor_shell() {
                let owner_hwnd = studio_native_hwnd(window);
                self.open_bridge_editor(track_id, insert_id, display_name, owner_hwnd, cx);
                return;
            }
            // The editor lives in a GPUI window: the plug-in's view is a child
            // of it, and the titlebar strip above carries the controls that
            // belong to the plug-in but cannot be drawn over its surface.
            //
            // Opened on a later turn of the event loop, never from here.
            // `open_window` reaches the platform, and creating a window
            // dispatches synchronous messages that re-enter GPUI; doing that
            // while a frame is anywhere on the stack draws a second window
            // inside the first one's draw. A spawned task is past the frame
            // entirely, which no `defer` from inside an update can promise.
            let owner_bounds = window.bounds();
            let track = track_id.to_string();
            let insert = insert_id.to_string();
            let executor = cx.background_executor().clone();
            cx.spawn(async move |this, cx| {
                executor.timer(std::time::Duration::from_millis(1)).await;
                let _ = this.update(cx, |layout, cx| {
                    layout.open_bridged_editor_window(
                        owner_bounds,
                        key,
                        track,
                        insert,
                        display_name,
                        cx,
                    );
                });
            })
            .detach();
            return;
        }

        // The editor attaches to the EXISTING runtime VST3 instance for this
        // insert — never a new component/controller. Look it up from the engine;
        // if the insert has no ready native processor, there is nothing to edit.
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            if debug {
                eprintln!("[plugin-view] no audio engine track={track_id} slot={insert_id}");
            }
            return;
        };
        let Some(processor) = engine.insert_processor(track_id, insert_id) else {
            if debug {
                eprintln!(
                    "[plugin-view] no ready runtime VST3 instance track={track_id} slot={insert_id} \
                     (insert not loaded / not native)"
                );
            }
            return;
        };

        let owner_bounds = window.bounds();
        match crate::components::plugin_editor_window::open_plugin_editor_window(
            owner_bounds,
            track_id.to_string(),
            insert_id.to_string(),
            display_name,
            Some(processor),
            None,
            // Ordinary inserts always go through the mandatory external bridge.
            false,
            cx,
        ) {
            Ok(handle) => {
                self.plugin_editors.open.insert(key, handle);
                if debug {
                    eprintln!("[plugin-view] open track={track_id} slot={insert_id}");
                }
            }
            Err(err) => {
                if debug {
                    eprintln!(
                        "[plugin-view] open FAILED track={track_id} slot={insert_id} err={err}"
                    );
                }
            }
        }
    }

    /// Build the sidebar's instance list for `plugin_id`: every insert slot
    /// across every track whose `plugin_id` matches, in track order. Cheap
    /// enough (project-sized, not audio-rate) to rebuild wholesale on every
    /// open/lifecycle event rather than diff in place.
    fn collect_builtin_instances(
        &self,
        plugin_id: &str,
        cx: &Context<Self>,
    ) -> Vec<crate::components::builtin_plugin_editor_window::PluginInstanceDescriptor> {
        use crate::components::builtin_plugin_editor_window::{
            PluginInstanceDescriptor, PluginInstanceKey,
        };

        let state = &self.timeline.read(cx).state;
        let mut instances = Vec::new();
        let owners = state
            .tracks
            .iter()
            .map(|track| (track.id.as_str(), track.name.as_str(), &track.inserts))
            .chain(std::iter::once((
                crate::components::timeline::timeline_state::MASTER_TRACK_ID,
                "Master",
                &state.master.inserts,
            )));
        for (track_id, track_name, inserts) in owners {
            for slot in inserts {
                let Some(slot_plugin_id) = slot.plugin_id.as_deref() else {
                    continue;
                };
                if slot_plugin_id != plugin_id {
                    continue;
                }
                // The live mirror is authoritative (it carries edits made
                // since the last save); the persisted blob covers a project
                // that was loaded but never edited this session — seed the
                // mirror from it so a later replay/save also sees it. `None`
                // only for a truly fresh insert (editor falls back to DSP
                // defaults).
                if let Some(persisted) = slot.vst3_state.as_deref() {
                    crate::components::builtin_plugin_editor::builtin_state_seed(
                        plugin_id, &slot.id, persisted,
                    );
                }
                let state_bytes = crate::components::builtin_plugin_editor::builtin_state_bytes(
                    plugin_id, &slot.id,
                )
                .map(std::sync::Arc::new)
                .or_else(|| slot.vst3_state.clone());
                instances.push(PluginInstanceDescriptor {
                    instance_key: PluginInstanceKey {
                        track_id: track_id.to_string(),
                        insert_id: slot.id.clone(),
                    },
                    plugin_id: plugin_id.to_string(),
                    track_name: track_name.to_string(),
                    insert_name: slot.display_name.clone(),
                    bypassed: slot.bypassed,
                    enabled: slot.enabled,
                    state_bytes,
                });
            }
        }
        instances
    }

    /// Open the CEF-hosted editor for a built-in plugin insert.
    ///
    /// Built-in editors are shared per `plugin_id`: one native window and one
    /// CEF browser serve every track/insert using that plugin. Opening a
    /// second insert of the same plugin_id focuses the existing window and
    /// switches its sidebar selection — it never creates a second browser.
    fn open_builtin_insert_editor(
        &mut self,
        track_id: &str,
        insert_id: &str,
        plugin_id: &str,
        display_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::components::builtin_plugin_editor_window::{
            BuiltinEditorHostOps, BuiltinGlobalCommandDispatcher, BuiltinHostStatusSource,
            BuiltinIrLoadForwarder, BuiltinIrLoadRequest, BuiltinMeterSource,
            BuiltinNamLoadForwarder, BuiltinNamLoadRequest, BuiltinParamForwarder,
            BuiltinSpectrumSource, BuiltinTransportSource, PluginInstanceKey,
        };

        let target = PluginInstanceKey {
            track_id: track_id.to_string(),
            insert_id: insert_id.to_string(),
        };
        let instances = self.collect_builtin_instances(plugin_id, cx);

        // Live param edits from the editor go to the engine's realtime command
        // path; the callback thread pushes them into this insert's shared
        // param ring (the ring's sole producer). `None` while the engine is
        // still warming up — the focus path re-installs a live one later.
        let studio = cx.weak_entity();
        let forward_param: Option<BuiltinParamForwarder> =
            self.audio_bridge.engine.clone().map(|engine| {
                let mirror_plugin_id = plugin_id.to_string();
                std::sync::Arc::new(
                    move |key: &PluginInstanceKey, index: u32, value: f32, cx: &mut App| {
                        // UI thread, non-realtime: string alloc + command send.
                        if let Err(error) = engine.set_insert_param(
                            key.track_id.clone(),
                            key.insert_id.clone(),
                            index.to_string(),
                            value,
                        ) {
                            eprintln!(
                                "[BuiltinPluginEditor] setParams forward failed insert={} index={index} error={error}",
                                key.insert_id
                            );
                        }
                        // Fold into the authoritative main-process mirror —
                        // the source for selectInstance state, project save,
                        // and host-restore replay.
                        crate::components::builtin_plugin_editor::builtin_state_apply(
                            &mirror_plugin_id,
                            &key.insert_id,
                            index,
                            value,
                        );
                        // The project now needs saving (view-only: the edit
                        // already reached the DSP, and the save flushes the
                        // mirror). Deferred: this runs inside the editor
                        // window's update, and the studio is its parent.
                        let studio = studio.clone();
                        cx.defer(move |cx| {
                            let _ = studio.update(cx, |layout, cx| {
                                layout.note_plugin_state_edited(cx);
                            });
                        });
                    },
                ) as BuiltinParamForwarder
            });

        // `.nam` loads and telemetry polls go through the plugin-host bridge
        // runtime (the DSP lives in the host process).
        let bridge_runtime = self.plugin_editors.bridge_runtime.clone();
        let load_nam_capture: Option<BuiltinNamLoadForwarder> =
            bridge_runtime.clone().map(|runtime| {
                std::sync::Arc::new(
                    move |key: &PluginInstanceKey, request: BuiltinNamLoadRequest| {
                        let command = SpherePluginHost::ipc::HostCommand::LoadBuiltinNamCapture {
                            plugin_instance_id: key.insert_id.clone(),
                            name: request.name,
                            json: request.json,
                            stereo: request.stereo,
                            full_rig: request.full_rig,
                        };
                        match runtime.lock() {
                            Ok(mut bridge) => {
                                if let Err(error) = bridge.send_raw(&command) {
                                    eprintln!(
                                        "[BuiltinPluginEditor] loadNamCapture send failed insert={} error={error}",
                                        key.insert_id
                                    );
                                }
                            }
                            Err(_) => eprintln!(
                                "[BuiltinPluginEditor] loadNamCapture dropped: bridge runtime poisoned"
                            ),
                        }
                    },
                ) as BuiltinNamLoadForwarder
            });
        let load_ir: Option<BuiltinIrLoadForwarder> = bridge_runtime.clone().map(|runtime| {
            std::sync::Arc::new(
                move |key: &PluginInstanceKey, request: BuiltinIrLoadRequest| {
                    use base64::Engine as _;
                    // Binary through a newline-framed JSON transport: base64
                    // here, decoded once in the host process.
                    let command = SpherePluginHost::ipc::HostCommand::LoadBuiltinIr {
                        plugin_instance_id: key.insert_id.clone(),
                        name: request.name,
                        wav_b64: base64::engine::general_purpose::STANDARD.encode(&request.bytes),
                    };
                    match runtime.lock() {
                        Ok(mut bridge) => {
                            if let Err(error) = bridge.send_raw(&command) {
                                eprintln!(
                                    "[BuiltinPluginEditor] loadIr send failed insert={} error={error}",
                                    key.insert_id
                                );
                            }
                        }
                        Err(_) => eprintln!(
                            "[BuiltinPluginEditor] loadIr dropped: bridge runtime poisoned"
                        ),
                    }
                },
            ) as BuiltinIrLoadForwarder
        });
        let meter_source: Option<BuiltinMeterSource> = bridge_runtime.clone().map(|runtime| {
            std::sync::Arc::new(move |key: &PluginInstanceKey| {
                runtime
                    .lock()
                    .ok()
                    .and_then(|bridge| bridge.builtin_meter_frame(&key.insert_id))
            }) as BuiltinMeterSource
        });
        let spectrum_source: Option<BuiltinSpectrumSource> =
            bridge_runtime.clone().map(|runtime| {
                std::sync::Arc::new(move |key: &PluginInstanceKey| {
                    runtime
                        .lock()
                        .ok()
                        .and_then(|bridge| bridge.builtin_spectrum_frame(&key.insert_id))
                }) as BuiltinSpectrumSource
            });
        let host_status_source: Option<BuiltinHostStatusSource> = bridge_runtime.map(|runtime| {
            std::sync::Arc::new(move |key: &PluginInstanceKey| {
                runtime
                    .lock()
                    .ok()
                    .and_then(|bridge| bridge.builtin_host_status(&key.insert_id))
            }) as BuiltinHostStatusSource
        });
        // Return leg of the editor's transport key: the editor sends
        // `transport:play-pause`, this reports what the transport actually does.
        // One relaxed atomic load, so the editor pump can poll it every tick.
        let transport_source: Option<BuiltinTransportSource> =
            self.audio_bridge.engine.clone().map(|engine| {
                std::sync::Arc::new(move || engine.transport_playing()) as BuiltinTransportSource
            });
        let command_owner = cx.entity().clone();
        let dispatch_global_command: Option<BuiltinGlobalCommandDispatcher> =
            Some(std::sync::Arc::new(move |command_id, cx| {
                let _ = command_owner.update(cx, |layout, cx| {
                    layout.dispatch_command_id(command_id, cx);
                    cx.notify();
                });
            }));
        let host_ops = BuiltinEditorHostOps {
            forward_param,
            dispatch_global_command,
            load_nam_capture,
            load_ir,
            meter_source,
            host_status_source,
            spectrum_source,
            transport_source,
        };

        // Focus the existing shared window and rebind it to this instance,
        // rather than creating a second browser for the same plugin_id.
        if let Some(handle) = self.plugin_editors.builtin.get(plugin_id) {
            let result = handle.update(cx, |editor, window, cx| {
                editor.set_host_ops(host_ops.clone());
                editor.set_instances(instances.clone(), cx);
                editor.select_instance(target.clone(), cx);
                window.activate_window();
            });
            if result.is_ok() {
                eprintln!(
                    "[BuiltinPluginEditor] focused shared plugin={plugin_id} track={track_id} insert={insert_id}"
                );
                return;
            }
            self.plugin_editors.builtin.remove(plugin_id);
        }

        let owner_bounds = window.bounds();
        match crate::components::builtin_plugin_editor_window::open_builtin_editor_window(
            owner_bounds,
            plugin_id.to_string(),
            display_name,
            instances,
            Some(target),
            host_ops,
            cx,
        ) {
            Ok(handle) => {
                self.plugin_editors
                    .builtin
                    .insert(plugin_id.to_string(), handle);
                eprintln!(
                    "[BuiltinPluginEditor] opened shared plugin={plugin_id} track={track_id} insert={insert_id}"
                );
            }
            Err(err) => {
                eprintln!(
                    "[BuiltinPluginEditor] open FAILED plugin={plugin_id} track={track_id} \
                     insert={insert_id} err={err}"
                );
            }
        }
    }

    /// Refresh every open shared built-in editor's sidebar from current
    /// project state. Call after any insert/track add/remove/rename/reorder
    /// (spec: sidebar must reflect the live project, not a stale snapshot
    /// taken at open time). Cheap no-op when no built-in editor is open.
    pub(super) fn refresh_builtin_editor_sidebars(&mut self, cx: &mut Context<Self>) {
        if self.plugin_editors.builtin.is_empty() {
            return;
        }
        let plugin_ids: Vec<String> = self.plugin_editors.builtin.keys().cloned().collect();
        for plugin_id in plugin_ids {
            let instances = self.collect_builtin_instances(&plugin_id, cx);
            if let Some(handle) = self.plugin_editors.builtin.get(&plugin_id) {
                let _ = handle.update(cx, |editor, _window, cx| {
                    editor.set_instances(instances, cx);
                });
            }
        }
    }

    /// Close the editor window for a slot if one is open. Idempotent. Removing
    /// the GPUI window drops the entity, which detaches the native view.
    pub(super) fn close_insert_editor(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        // Native main-owned bridge editor (default path).
        self.close_bridge_editor(cx, track_id, insert_id);
        // Legacy GPUI-window editor (FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS only).
        let key = (track_id.to_string(), insert_id.to_string());
        // Built-in CEF editor: this instance is gone (unloaded/replaced), but
        // the shared browser other instances of the same plugin_id use must
        // not be torn down. Refresh every open shared editor's sidebar so it
        // drops the now-gone instance (and reselects/clears active selection
        // if it was the one showing) instead of destroying the window.
        self.refresh_builtin_editor_sidebars(cx);
        if let Some(handle) = self.plugin_editors.open.remove(&key) {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            eprintln!("[PluginEditorClose] plugin={insert_id} removed_called=true");
            if std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some() {
                eprintln!("[plugin-view] close track={track_id} slot={insert_id}");
            }
        }
    }

    pub(super) fn unload_bridge_plugin(&mut self, insert_id: &str) {
        if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
            if let Ok(mut runtime) = runtime.lock() {
                runtime.unload_plugin(insert_id.to_string());
            }
        }
    }

    /// Fully tear down ONE live plugin instance everywhere outside the project
    /// model — editor window, external bridge-host instance, and the engine's
    /// realtime bridge-audio sink — so no registry keeps the old
    /// `PluginInstanceId` alive. Call this BEFORE dropping the slot from the
    /// model. Idempotent: safe for an instance that is already gone.
    ///
    /// The in-process VST3 graph node is released separately: dropping the slot
    /// and re-syncing makes the engine reconcile drop its processor clone
    /// (`sphere_daux_vst3_destroy`). Fresh slot ids (see
    /// `TimelineState::next_insert_slot_id`) guarantee the reconcile can never
    /// reuse the dropped instance for the next add.
    pub(super) fn teardown_insert_instance(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
        reason: &'static str,
    ) {
        use crate::components::timeline::timeline_state::TrackType;
        eprintln!("[PluginUnload] start track={track_id} instance={insert_id} reason={reason}");

        // Only the instrument (inserts[0] on an Instrument/MIDI track) carries
        // MIDI — panic the track so a held/stuck note can't sustain past the
        // unload. Cheap no-op for an effect insert.
        let is_instrument = self
            .timeline
            .read(cx)
            .state
            .find_track(track_id)
            .map(|track| {
                track.instrument_plugin_instance_id.as_deref() == Some(insert_id)
                    || (matches!(track.track_type, TrackType::Instrument | TrackType::Midi)
                        && track
                            .inserts
                            .first()
                            .map(|slot| slot.id == insert_id)
                            .unwrap_or(false))
            })
            .unwrap_or(false);

        // 1. Editor window (native main-owned bridge shell + legacy GPUI) —
        //    disconnects the editor from the instance and releases its clone.
        self.close_insert_editor(track_id, insert_id, cx);
        // An editor request may have been queued while the bridge instance was
        // still loading. Instance removal is authoritative: discard every
        // deferred/open-loop reference before a late PluginLoaded event can
        // resurrect an editor for a slot that no longer exists.
        self.plugin_editors.discard_pending_open(insert_id);
        eprintln!("[PluginUnload] editor_closed track={track_id} instance={insert_id}");

        // 2. External bridge host: real UnloadPlugin (host closes the editor,
        //    suspends + deactivates the VST3 component, releases component /
        //    controller, drops HWND / param / MIDI maps) and drops the bridge
        //    runtime's shared-audio region for this instance.
        self.unload_bridge_plugin(insert_id);
        eprintln!("[PluginUnload] host_unload_sent track={track_id} instance={insert_id}");

        // 3. Engine realtime sink: keyed by instance id and PRESERVED across
        //    LoadProject, so the snapshot reconcile alone never drops it. Remove
        //    it explicitly or the removed plugin keeps mixing into the master.
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            match engine.set_plugin_bridge_sink(insert_id.to_string(), None) {
                Ok(()) => eprintln!(
                    "[PluginUnload] engine_sink_removed track={track_id} instance={insert_id}"
                ),
                Err(error) => eprintln!(
                    "[PluginUnload] engine_sink_remove_failed instance={insert_id} err={error}"
                ),
            }
            if is_instrument {
                match engine.midi_preview_all_notes_off(track_id.to_string()) {
                    Ok(()) => eprintln!(
                        "[PluginUnload] midi_disconnected track={track_id} instance={insert_id}"
                    ),
                    Err(error) => eprintln!(
                        "[PluginUnload] midi_panic_failed track={track_id} instance={insert_id} err={error}"
                    ),
                }
            }
        }

        // 4. Built-in state mirror: this instance id is dead; a future insert
        //    always gets a fresh id, so the entry would only leak.
        crate::components::builtin_plugin_editor::builtin_state_remove(insert_id);

        eprintln!("[PluginUnload] complete track={track_id} instance={insert_id}");
    }

    /// RemoveInstrumentPlugin / remove-insert flow: tear the live instance down
    /// everywhere, drop the slot from the project model, then push the new
    /// snapshot so the engine reconcile destroys the in-process VST3 clone. The
    /// next add always receives a fresh `PluginInstanceId`, so nothing is reused.
    pub(super) fn remove_insert_fully(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
        reason: &'static str,
    ) {
        // One undo step, which brings the plug-in back as it sounds now.
        self.store_live_plugin_states(track_id, std::slice::from_ref(&insert_id.to_string()), cx);
        let before = self.capture_insert_chains(&[track_id], cx);
        self.teardown_insert_instance(track_id, insert_id, cx, reason);
        self.timeline.update(cx, |timeline, cx| {
            timeline.state.remove_insert(track_id, insert_id);
            cx.notify();
        });
        self.record_insert_chains("Remove Plug-in", before, cx);
        self.mark_dirty();
        self.audio_bridge.project_dirty = true;
        // Push the snapshot now instead of waiting for the idle poll: the engine
        // reconcile drops the old processor clone and the sink removal applies
        // immediately, so the removed VSTi can never sound again.
        self.schedule_audio_project_sync(cx, true, reason);
        self.assert_instance_fully_removed(track_id, insert_id, cx);
        cx.notify();
    }

    /// Post-removal invariant check (logged; debug-asserted). Proves the
    /// `PluginInstanceId` is gone from every registry the main app owns. The
    /// in-process engine graph node + MIDI router live on the audio thread and
    /// are verified by the engine reconcile log, not from here.
    pub(super) fn assert_instance_fully_removed(
        &self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) {
        let (slot_present, instrument_ptr) = {
            let state = &self.timeline.read(cx).state;
            let track = state.find_track(track_id);
            let slot_present = track
                .map(|t| t.inserts.iter().any(|s| s.id == insert_id))
                .unwrap_or(false);
            let instrument_ptr = track
                .map(|t| t.instrument_plugin_instance_id.as_deref() == Some(insert_id))
                .unwrap_or(false);
            (slot_present, instrument_ptr)
        };
        let editor_open = self
            .plugin_editors
            .open
            .keys()
            .chain(self.plugin_editors.bridge.keys())
            .any(|(_, id)| id == insert_id);
        let editor_deferred = self
            .plugin_editors
            .deferred_opens
            .iter()
            .any(|(_, _, id)| id == insert_id)
            || self.plugin_editors.flush_attempts.contains_key(insert_id);
        let bridge_loaded = self
            .plugin_editors
            .bridge_runtime
            .as_ref()
            .and_then(|runtime| runtime.lock().ok().map(|r| r.is_loaded(insert_id)))
            .unwrap_or(false);
        eprintln!(
            "[PluginUnload] invariants track={track_id} instance={insert_id} \
             slot_present={slot_present} instrument_ptr={instrument_ptr} \
             editor_open={editor_open} editor_deferred={editor_deferred} bridge_loaded={bridge_loaded}"
        );
        debug_assert!(!slot_present, "insert slot still present after removal");
        debug_assert!(
            !instrument_ptr,
            "instrument pointer still set after removal"
        );
        debug_assert!(!editor_open, "editor still open after removal");
        debug_assert!(!editor_deferred, "editor open still deferred after removal");
        debug_assert!(!bridge_loaded, "bridge instance still loaded after removal");
    }

    /// CloseProject flow: tear down EVERY live plugin instance before the
    /// project model is replaced (load / close). The engine preserves its
    /// bridge-sink map across `LoadProject`, so without this every old
    /// instance's sink + bridge-host process + editor leaks into the next
    /// project.
    pub(super) fn teardown_all_plugin_instances(
        &mut self,
        cx: &mut Context<Self>,
        reason: &'static str,
    ) {
        let pairs: Vec<(String, String)> = {
            let state = &self.timeline.read(cx).state;
            let mut pairs: Vec<(String, String)> = state
                .tracks
                .iter()
                .flat_map(|track| {
                    track
                        .inserts
                        .iter()
                        .map(move |slot| (track.id.clone(), slot.id.clone()))
                })
                .collect();
            // Defensive: also catch any editor whose model slot already vanished.
            for key in self
                .plugin_editors
                .open
                .keys()
                .chain(self.plugin_editors.bridge.keys())
            {
                if !pairs.contains(key) {
                    pairs.push(key.clone());
                }
            }
            pairs
        };
        eprintln!(
            "[ProjectClose] teardown_all instances={} reason={reason}",
            pairs.len()
        );
        for (track_id, insert_id) in pairs {
            self.teardown_insert_instance(&track_id, &insert_id, cx, reason);
        }
    }

    /// Close any open plugin editor whose owning track or insert slot no longer
    /// exists in the project model. This is the catch-all that backstops every
    /// removal path — the track-header delete button mutates the `Timeline`
    /// entity directly (it cannot reach `StudioLayout`'s editor registry), and
    /// undo/redo or programmatic edits can also drop a track/insert without
    /// going through `cleanup_track_plugins_before_delete`. Without this, a
    /// deleted track leaves an orphan editor window holding a live
    /// `Vst3RuntimeProcessor` clone, which keeps the C++ VST3 instance from ever
    /// being destroyed (Part 11). Cheap: only inspects state when an editor is
    /// open, and only closes when its backing slot is genuinely gone.
    pub(super) fn reconcile_open_plugin_editors(&mut self, cx: &mut Context<Self>) {
        // Shared built-in editors: same backstop, but they track membership
        // (not existence) — a deleted track/insert must drop out of the
        // sidebar even if nothing ever called `close_insert_editor` for it.
        self.refresh_builtin_editor_sidebars(cx);
        if self.plugin_editors.open.is_empty() && self.plugin_editors.bridge.is_empty() {
            return;
        }
        let stale: Vec<(String, String)> = {
            let state = &self.timeline.read(cx).state;
            let is_stale = |(track_id, insert_id): &&(String, String)| {
                // An ARA editor is filed here under an `ara:` key so it sits
                // beside the insert editors, but it is bound to a clip and has
                // no insert slot to look up — every one of them would read as
                // stale and be torn down the moment it opened. Its lifetime is
                // the ARA session's (`ara_studio`), not a slot's.
                if is_ara_editor_key(insert_id) {
                    return false;
                }
                state.find_insert_slot(track_id, insert_id).is_none()
            };
            self.plugin_editors
                .open
                .keys()
                .filter(is_stale)
                .chain(self.plugin_editors.bridge.keys().filter(is_stale))
                .cloned()
                .collect()
        };
        if stale.is_empty() {
            return;
        }
        // A stale key whose insert still lives on another channel was moved
        // there, not removed: `teardown_insert_instance` unloads by instance id
        // alone and would silence it where it now plays. Its editor caches are
        // re-filed instead.
        let removed: Vec<(String, String)> = {
            let state = &self.timeline.read(cx).state;
            stale
                .iter()
                .filter(|(track_id, insert_id)| {
                    classify_insert_key(state, track_id, insert_id) == InsertKeyStatus::Removed
                })
                .cloned()
                .collect()
        };
        if removed.len() < stale.len() {
            self.reconcile_insert_ownership(cx);
        }
        for (track_id, insert_id) in removed {
            eprintln!(
                "[PluginUnload] track_id={track_id} insert_id={insert_id} action=teardown_instance reason=stale_reference"
            );
            // This path runs after a direct model mutation (track-header delete,
            // undo/redo, or programmatic replacement), so closing only the
            // window is insufficient. Remove the host instance and engine sink
            // by the same stable instance id used by explicit remove flows.
            self.teardown_insert_instance(&track_id, &insert_id, cx, "stale_reference");
        }
    }

    /// Re-derive every cache that addresses an insert as `(track_id,
    /// insert_id)` from where each insert lives now. Runs after an insert
    /// chain edit and its undo/redo — a moved insert keeps its instance id,
    /// and the engine drops parameter and enable commands whose track does not
    /// match the insert's, so every cache still naming the old channel would
    /// fail silently.
    ///
    /// * Editor windows are one per channel, one tab per plug-in. A plug-in
    ///   that left the channel leaves its tab: the window brings a remaining
    ///   tab to the front (releasing the moved plug-in's view, which does not
    ///   unload it), re-files itself under a remaining tab if it was filed
    ///   under the moved one, or closes when nothing is left. A window is never
    ///   re-filed under another channel.
    /// * A bridge editor session is one plug-in's own shell: closed without
    ///   unloading, or re-filed under the new owner while it is still a
    ///   loading shell.
    /// * Preset selection and deferred editor opens follow the insert (a
    ///   deferred open also takes the insert's current slot index).
    /// * Bridge descriptors take the new track (MIDI fallback routes by it).
    /// * Built-in editors rebuild their sidebar; a moved active instance stays
    ///   selected under its new key.
    pub(super) fn reconcile_insert_ownership(&mut self, cx: &mut Context<Self>) {
        use crate::components::timeline::timeline_state::MASTER_TRACK_ID;

        let owners: std::collections::HashMap<String, (String, usize)> = {
            let state = &self.timeline.read(cx).state;
            let tracks = state
                .tracks
                .iter()
                .map(|track| (track.id.as_str(), &track.inserts))
                .chain(std::iter::once((MASTER_TRACK_ID, &state.master.inserts)));
            let mut owners = std::collections::HashMap::new();
            for (track_id, inserts) in tracks {
                for (index, slot) in inserts.iter().enumerate() {
                    owners.insert(slot.id.clone(), (track_id.to_string(), index));
                }
            }
            owners
        };
        let owner_of = |insert_id: &str| owners.get(insert_id).map(|(track, _)| track.as_str());
        let left = |track_id: &str, insert_id: &str| {
            owner_of(insert_id).is_some_and(|owner| owner != track_id)
        };

        // Channel editor windows.
        let windows: Vec<_> = self
            .plugin_editors
            .open
            .iter()
            .filter(|((_, key_insert), _)| !is_ara_editor_key(key_insert))
            .map(|(key, handle)| (key.clone(), *handle))
            .collect();
        for ((track_id, key_insert), handle) in windows {
            let active = handle
                .update(cx, |editor, _window, _cx| editor.insert_key().1.to_string())
                .ok();
            let Some(active) = active else {
                // The window is gone; only drop a handle that is stale anyway.
                if left(&track_id, &key_insert) {
                    self.plugin_editors
                        .open
                        .remove(&(track_id.clone(), key_insert.clone()));
                }
                continue;
            };
            let tabs = self
                .plugin_editors
                .editor_tabs
                .get(&track_id)
                .cloned()
                .unwrap_or_default();
            let Some(plan) =
                plan_editor_tab_detach(&tabs, &key_insert, &active, |id| left(&track_id, id))
            else {
                continue;
            };
            eprintln!(
                "[plugin-editor-window] channel={track_id} lost moved insert(s); \
                 remaining_tabs={} close={}",
                plan.remaining.len(),
                plan.close_window
            );
            let key = (track_id.clone(), key_insert.clone());
            if plan.close_window {
                self.plugin_editors.editor_tabs.remove(&track_id);
                self.plugin_editors.open.remove(&key);
                // The preset list is a window of its own; it goes first. Dropping
                // the editor releases the view without unloading the plug-in.
                let _ = handle.update(cx, |editor, window, cx| {
                    editor.close_preset_menu(cx);
                    window.remove_window();
                });
                continue;
            }
            self.plugin_editors
                .editor_tabs
                .insert(track_id.clone(), plan.remaining);
            if let Some(next) = plan.rekey {
                if let Some(handle) = self.plugin_editors.open.remove(&key) {
                    self.plugin_editors
                        .open
                        .insert((track_id.clone(), next), handle);
                }
            }
            if let Some(next) = plan.activate {
                self.select_plugin_editor_tab(&track_id, &next, cx);
            }
        }

        // Bridge editor sessions. An open shell is the moved plug-in's own
        // editor: closed through the path that leaves the instance running
        // (and clears the old track's editor flag in the engine). A shell still
        // waiting for the plug-in to load is re-filed instead, so the deferred
        // open — re-filed below as well — replaces it on the new channel.
        let moved_sessions: Vec<((String, String), bool)> = self
            .plugin_editors
            .bridge
            .iter()
            .filter(|((track_id, insert_id), _)| left(track_id, insert_id))
            .map(|(key, session)| (key.clone(), session.state == BridgeEditorState::Loading))
            .collect();
        for ((track_id, insert_id), loading) in moved_sessions {
            if !loading {
                self.close_bridge_editor(cx, &track_id, &insert_id);
                continue;
            }
            let Some(owner) = owner_of(&insert_id).map(str::to_string) else {
                continue;
            };
            if let Some(mut session) = self
                .plugin_editors
                .bridge
                .remove(&(track_id, insert_id.clone()))
            {
                session.track_id = owner.clone();
                self.plugin_editors
                    .bridge
                    .insert((owner, insert_id), session);
            }
        }

        // Preset selection.
        let moved_presets: Vec<(String, String)> = self
            .plugin_editors
            .preset_selection
            .keys()
            .filter(|(track_id, insert_id)| left(track_id, insert_id))
            .cloned()
            .collect();
        for key in moved_presets {
            let Some(owner) = owner_of(&key.1).map(str::to_string) else {
                continue;
            };
            if let Some(index) = self.plugin_editors.preset_selection.remove(&key) {
                self.plugin_editors
                    .preset_selection
                    .insert((owner, key.1), index);
            }
        }

        // Deferred editor opens.
        for (track_id, slot_index, insert_id) in &mut self.plugin_editors.deferred_opens {
            if let Some((owner, index)) = owners.get(insert_id.as_str()) {
                track_id.clone_from(owner);
                *slot_index = *index;
            }
        }

        // Bridge descriptors.
        if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
            if let Ok(mut runtime) = runtime.lock() {
                let changed = runtime.retarget_tracks(|insert_id| owner_of(insert_id));
                if changed > 0 {
                    eprintln!(
                        "[plugin-bridge] retargeted {changed} instance(s) to their owning track"
                    );
                }
            }
        }

        self.refresh_builtin_editor_sidebars(cx);
        self.refresh_plugin_editor_chrome(cx);
    }

    /// Close every open plugin editor and release native embed sessions before
    /// application exit (avoids HWND/VST3 teardown during TLS destruction).
    pub(super) fn shutdown_plugin_editors(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<(String, String)> = self
            .plugin_editors
            .open
            .keys()
            .chain(self.plugin_editors.bridge.keys())
            .cloned()
            .collect();
        for (track_id, insert_id) in keys {
            self.close_insert_editor(&track_id, &insert_id, cx);
        }
        // Shared built-in editors are keyed by plugin_id, not (track, insert),
        // so they need their own close pass — `close_insert_editor` only
        // refreshes their sidebars, it never tears the shared window down.
        for (plugin_id, handle) in self.plugin_editors.builtin.drain() {
            let _ = handle.update(cx, |editor, _window, cx| editor.request_close(cx));
            eprintln!(
                "[BuiltinPluginEditorClose] plugin={plugin_id} close_requested=true shutdown=true"
            );
        }
        SpherePluginHost::native_editor::detach_all_embedded_editors();
    }

    /// Start a background SQLite load of the plug-in catalog. The picker
    /// opens instantly with a skeleton; this task replaces the skeleton once
    /// the catalog is read. Re-entrant: a second call while a load is in
    /// flight is a no-op.
    ///
    /// **Never** invokes the VST3/CLAP scanner; **never** touches plug-in
    /// binaries. The picker's open path must stay UI-only.
    pub(super) fn arm_catalog_load(&mut self, cx: &mut Context<Self>) {
        // Already loaded and not stale → nothing to do.
        if matches!(self.plugin_catalog.status, PluginCatalogStatus::Ready)
            && self.plugin_catalog.available.is_some()
        {
            return;
        }
        if matches!(self.plugin_catalog.status, PluginCatalogStatus::Loading)
            && self.plugin_catalog.available.is_none()
        {
            // Spawn-in-progress (initial boot path also fires this).
        } else {
            self.plugin_catalog.status = PluginCatalogStatus::Loading;
        }

        let debug = std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some()
            || std::env::var_os("FUTUREBOARD_PLUGIN_DB_DEBUG").is_some();
        let shell_started = std::time::Instant::now();

        cx.spawn(async move |this, cx| {
            let load = cx
                .background_executor()
                .spawn(async { SpherePluginHost::PluginRegistry::load_catalog() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if crate::shutdown::ShutdownState::global().is_shutting_down() {
                    return;
                }
                match load {
                    CatalogLoad::Loaded { catalog, sqlite_ms } => {
                        let count = catalog.plugins.len();
                        let scanned: Vec<SpherePluginHost::RegistryPlugin> = catalog
                            .plugins
                            .iter()
                            .map(|e| e.to_registry_plugin())
                            .collect();
                        // Surface the Futureboard built-in (stock) plug-ins next to
                        // the scanned VST3/CLAP rows so they appear in the picker and
                        // the "Built-in" format facet even before any external scan.
                        let plugins = SpherePluginHost::with_builtins(scanned, now_ms_i64());
                        this.plugin_catalog.available = Some(plugins.clone());
                        this.plugin_search_index =
                            Some(std::sync::Arc::new(PluginSearchIndex::from_plugins(plugins)));
                        this.plugin_picker_au_error = load_au_cache_state().last_error;
                        this.plugin_catalog.cache_present = true;
                        this.plugin_catalog.status = PluginCatalogStatus::Ready;
                        this.update_add_track_instrument_plugins(cx);
                        if debug {
                            eprintln!(
                                "[plugin-db] loaded rows={count} sqlite_ms={sqlite_ms} path={} total_ms={}",
                                catalog.source_path.display(),
                                shell_started.elapsed().as_millis(),
                            );
                        }
                    }
                    CatalogLoad::MissingDatabase { path } => {
                        // Built-ins are always available, even with no scan database.
                        let plugins = SpherePluginHost::with_builtins(Vec::new(), now_ms_i64());
                        this.plugin_search_index =
                            Some(std::sync::Arc::new(PluginSearchIndex::from_plugins(plugins.clone())));
                        this.plugin_catalog.available = Some(plugins);
                        this.plugin_catalog.cache_present = false;
                        this.plugin_catalog.status = PluginCatalogStatus::MissingDatabase;
                        this.update_add_track_instrument_plugins(cx);
                        if debug {
                            eprintln!(
                                "[plugin-db] path={} exists=false",
                                path.display()
                            );
                        }
                    }
                    CatalogLoad::Error { path, message } => {
                        // Keep built-ins usable even when the scan cache failed to load.
                        let plugins = SpherePluginHost::with_builtins(Vec::new(), now_ms_i64());
                        this.plugin_search_index =
                            Some(std::sync::Arc::new(PluginSearchIndex::from_plugins(plugins.clone())));
                        this.plugin_catalog.available = Some(plugins);
                        this.plugin_catalog.cache_present = path.exists();
                        this.plugin_catalog.status =
                            PluginCatalogStatus::Error(message.clone());
                        this.update_add_track_instrument_plugins(cx);
                        if debug {
                            eprintln!(
                                "[plugin-db] error path={} message={}",
                                path.display(),
                                message
                            );
                        }
                    }
                }
                this.notify_insert_picker_window(cx);
                // ARA sessions of a project restored before the catalog had
                // loaded open now, rather than reporting their plug-in missing.
                this.open_pending_ara_sessions(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Open the Phase 2b insert picker for `track_id`. Loads from cached
    /// `.pst` index only (no VST3/CLAP scan, no plug-in binary read) so the
    /// overlay opens instantly even with 1000+ plug-ins. No insert slot is
    /// created until the user picks a plugin.
    pub(super) fn open_insert_picker(
        &mut self,
        track_id: &str,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        self.open_insert_picker_for(track_id, None, PluginInsertKind::Effect, window, cx);
    }

    pub(super) fn open_insert_picker_for(
        &mut self,
        track_id: &str,
        slot_index: Option<usize>,
        desired_kind: PluginInsertKind,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        use crate::components::timeline::timeline_state::{MASTER_TRACK_ID, TrackType};

        let debug = std::env::var_os("FUTUREBOARD_PLUGIN_PICKER_DEBUG").is_some();
        let started = std::time::Instant::now();
        let track_info = {
            let timeline = self.timeline.read(cx);
            if track_id == MASTER_TRACK_ID {
                Some((
                    "Master".to_string(),
                    TrackType::Master,
                    timeline.state.master.inserts.len(),
                ))
            } else {
                timeline
                    .state
                    .find_track(track_id)
                    .map(|track| (track.name.clone(), track.track_type, track.inserts.len()))
            }
        };
        let (track_name, track_type, next_slot) =
            track_info.unwrap_or((track_id.to_string(), TrackType::Audio, 0));
        let target_slot = slot_index.unwrap_or_else(|| {
            if track_type == TrackType::Instrument && desired_kind == PluginInsertKind::Effect {
                next_slot.max(1)
            } else {
                next_slot
            }
        });
        let filter = match desired_kind {
            PluginInsertKind::Instrument => PickerFilter::Instruments,
            PluginInsertKind::Effect => PickerFilter::Effects,
        };
        self.plugin_picker = PluginPickerState::open_for_with_filter(
            track_id,
            &track_name,
            track_type,
            target_slot,
            self.plugin_picker_prefs.show_details,
            filter,
            desired_kind,
        );
        self.plugin_picker_search_input.set_value("");
        self.plugin_picker.query = String::new();
        let owner_bounds = window.as_ref().map(|w| w.bounds());
        if let Some(window) = window {
            self.plugin_picker_search_input
                .focus_handle
                .focus(window, cx);
        }
        if let Some(index) = self.plugin_search_index.as_ref() {
            ensure_default_highlight(&mut self.plugin_picker, index, &self.plugin_picker_prefs);
        }
        // Start (or rejoin) the background SQLite load. Picker shell is
        // visible immediately; skeleton rows fill in until the catalog lands.
        if self.plugin_catalog.available.is_none()
            || !matches!(self.plugin_catalog.status, PluginCatalogStatus::Ready)
        {
            self.arm_catalog_load(cx);
        }
        eprintln!(
            "[plugin-picker] opened track={} slot={} kind={:?}",
            track_id, target_slot, desired_kind
        );
        // Do not force a secondary format filter here — sidebar Format / Built-in
        // facets already gate by format, and a latent VST3-only override would
        // empty the Built-in / CLAP / AU lists while their counts still look right.
        self.plugin_picker.filters.format = None;
        if debug {
            let state_label = match &self.plugin_catalog.status {
                PluginCatalogStatus::Loading => "LoadingCatalog",
                PluginCatalogStatus::Ready => "Ready",
                PluginCatalogStatus::MissingDatabase => "MissingDatabase",
                PluginCatalogStatus::Error(_) => "Error",
            };
            eprintln!(
                "[plugin-picker] opened state={state_label} shell_ms={}",
                started.elapsed().as_millis()
            );
        }
        self.open_insert_picker_external_window(owner_bounds, cx);
        cx.notify();
    }

    /// Track-header shortcut: open the instrument already on this track, or
    /// the instrument picker when the slot is empty.
    pub(super) fn open_track_instrument(
        &mut self,
        track_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use super::WorkspaceActivePanel;
        use crate::components::timeline::timeline_state::TrackType;

        let info = self
            .timeline
            .read(cx)
            .state
            .find_track(track_id)
            .map(|track| {
                (
                    track.track_type,
                    track.solfege.is_some(),
                    track.builtin_soundfont_player,
                    track
                        .instrument_insert()
                        .filter(|slot| !slot.is_empty())
                        .map(|slot| slot.id.clone()),
                )
            });
        let Some((track_type, has_solfege, has_soundfont, insert_id)) = info else {
            return;
        };
        if track_type != TrackType::Instrument {
            return;
        }
        if has_solfege {
            self.panels.inspector = true;
            self.set_active_panel(WorkspaceActivePanel::Solfege, cx);
            cx.notify();
            return;
        }
        if has_soundfont {
            self.open_soundfont_player_window(Some(window.bounds()), track_id.to_string(), cx);
            return;
        }
        if let Some(insert_id) = insert_id {
            self.open_insert_editor(track_id, 0, &insert_id, window, cx);
            return;
        }
        self.open_insert_picker_for(
            track_id,
            Some(0),
            PluginInsertKind::Instrument,
            Some(window),
            cx,
        );
    }

    /// Apply a picked plugin: append an insert slot to the picker's target
    /// track and bind the chosen descriptor. `plugin_id` is a
    /// `RegistryPlugin.id` or [`STUB_PLUGIN_ID`]. Closes the picker. No audio
    /// thread interaction — the next project sync carries the descriptor down.
    pub(super) fn apply_picked_insert(
        &mut self,
        plugin_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<(String, usize, String)> {
        use crate::components::plugin_picker::validate_insert;
        use crate::components::timeline::timeline_state::InsertPluginFormat;
        use SpherePluginHost::PluginFormat as RegFmt;

        let track_id = self.plugin_picker.insert_target.track_id.clone();
        let target_slot_index = self.plugin_picker.insert_target.next_slot_index;
        let desired_kind = self.plugin_picker.insert_target.desired_kind;
        if track_id.is_empty() {
            self.plugin_picker = PluginPickerState::closed();
            cx.notify();
            return None;
        }

        if plugin_id == STUB_PLUGIN_ID {
            eprintln!("[PluginAdd] rejected stub plugin selection");
            self.plugin_picker = PluginPickerState::closed();
            cx.notify();
            return None;
        }

        if plugin_id != STUB_PLUGIN_ID {
            if let Some(plugins) = self.plugin_catalog.available.as_ref() {
                if let Some(reg) = plugins.iter().find(|p| p.id == plugin_id) {
                    // Built-ins use PluginFormat::Unknown + a `builtin:` id; allow
                    // anything `supports_insert` / validate_insert already accepts
                    // (VST3, CLAP, built-in) rather than a VST3-only gate.
                    if validate_insert(reg, &self.plugin_picker.insert_target)
                        != crate::components::plugin_picker::InsertValidation::Ok
                    {
                        eprintln!(
                            "[PluginAdd] rejected plugin id={} format={:?} builtin={}",
                            reg.id,
                            reg.format,
                            reg.is_builtin()
                        );
                        cx.notify();
                        return None;
                    }
                }
            }
        }

        let descriptor = if plugin_id == STUB_PLUGIN_ID {
            None
        } else {
            self.plugin_catalog
                .available
                .as_ref()
                .and_then(|plugins| plugins.iter().find(|p| p.id == plugin_id))
                .map(|reg| {
                    // Exhaustive on purpose: a format that falls through to
                    // `Unknown` here is never recognised as bridge-hosted, so it
                    // silently never loads and its editor never opens.
                    let format = match reg.format {
                        RegFmt::Vst3 => InsertPluginFormat::Vst3,
                        RegFmt::Vst2 => InsertPluginFormat::Vst2,
                        RegFmt::Clap => InsertPluginFormat::Clap,
                        RegFmt::Au => InsertPluginFormat::Au,
                        RegFmt::Lv2 => InsertPluginFormat::Lv2,
                        RegFmt::Unknown => InsertPluginFormat::Unknown,
                    };
                    let id = reg.class_id.clone().unwrap_or_else(|| reg.id.clone());
                    (
                        id,
                        Some(reg.path.clone()),
                        format,
                        Some(reg.vendor.clone()).filter(|vendor| !vendor.trim().is_empty()),
                        reg.name.clone(),
                        reg.kind == SpherePluginHost::PluginKind::Instrument,
                    )
                })
        };
        let Some((
            plugin_id_out,
            plugin_path,
            plugin_format,
            vendor,
            display_name,
            plugin_is_instrument,
        )) = descriptor
        else {
            eprintln!(
                "[PluginAdd] plugin instance failed to create reason=plugin_not_in_registry id={plugin_id}"
            );
            self.plugin_picker = PluginPickerState::closed();
            cx.notify();
            return None;
        };

        // Replace flow: if the target slot already holds a loaded plugin, this
        // is a replace-on-top. Fully tear the OLD instance down (editor + bridge
        // host + engine sink) and give the slot a FRESH id, so the engine
        // reconcile can never reuse the previous instance and the same plugin
        // file loads as an independent instance. A fresh/empty slot just gets a
        // new slot id as before.
        let existing_slot_id = self
            .timeline
            .read(cx)
            .state
            .insert_slot_at(&track_id, target_slot_index)
            .filter(|slot| !slot.is_empty())
            .map(|slot| slot.id.clone());
        // One undo step. A replaced plug-in comes back as it sounds now.
        let history_label = if existing_slot_id.is_some() {
            "Replace Plug-in"
        } else {
            "Add Plug-in"
        };
        if let Some(old_slot_id) = existing_slot_id.as_ref() {
            self.store_live_plugin_states(&track_id, std::slice::from_ref(old_slot_id), cx);
        }
        let history_before = self.capture_insert_chains(&[track_id.as_str()], cx);
        let new_slot_id = if let Some(old_slot_id) = existing_slot_id {
            self.teardown_insert_instance(&track_id, &old_slot_id, cx, "replace_instrument_plugin");
            self.timeline.update(cx, |timeline, _cx| {
                timeline
                    .state
                    .replace_insert_with_fresh_slot(&track_id, &old_slot_id)
            })
        } else {
            self.timeline.update(cx, |timeline, _cx| {
                timeline
                    .state
                    .ensure_insert_slot_at(&track_id, target_slot_index)
            })
        };
        let mut opened_slot = None;
        if let Some(slot_id) = new_slot_id {
            // Defensive: a fresh slot never has an editor open, and the replace
            // path already closed the old one — but closing the (new) slot id is
            // a cheap no-op that keeps every add paired with a close.
            self.close_insert_editor(&track_id, &slot_id, cx);
            let log_display_name = display_name.clone();
            eprintln!(
                "[PluginAdd] plugin selected format={} id={plugin_id} name={log_display_name}",
                plugin_format.label()
            );
            eprintln!("[PluginAdd] track={track_id} slot={slot_id} plugin={log_display_name}");
            eprintln!("[PluginAdd] insert added to track track={track_id} slot={slot_id}");
            eprintln!("[PluginAdd] runtime_instance_id={slot_id}");
            let bridge_class_id = plugin_id_out.clone();
            self.timeline.update(cx, |timeline, _cx| {
                timeline.state.set_insert_plugin(
                    &track_id,
                    &slot_id,
                    plugin_id_out,
                    plugin_path,
                    plugin_format,
                    vendor,
                    display_name,
                );
                timeline
                    .state
                    .set_insert_plugin_role(&track_id, &slot_id, plugin_is_instrument);
            });
            let is_builtin = SpherePluginHost::builtin_audio_bridge_supported(&bridge_class_id);
            let is_audio_unit = !is_builtin && plugin_format == InsertPluginFormat::Au;
            let bridge_enabled = super::plugin_bridge_runtime::bridge_enabled()
                && (is_builtin
                    || is_audio_unit
                    || matches!(
                        plugin_format,
                        InsertPluginFormat::Vst3
                            | InsertPluginFormat::Vst2
                            | InsertPluginFormat::Clap
                    ));
            if bridge_enabled {
                use crate::components::timeline::timeline_state::{
                    PluginRuntimeBackend, PluginRuntimeState,
                };
                eprintln!("[plugin-runtime] backend=external_bridge reason=forced_default");
                // Effects and instruments both load through the bridge, so both
                // get the indeterminate load dialog — a click that appears to do
                // nothing for ten seconds is the confusing part, not the format.
                // Built-ins are excluded: they come up in milliseconds, and a
                // dialog that only flashes is noise rather than reassurance.
                if !is_builtin {
                    self.begin_plugin_load_progress(&slot_id, &log_display_name, cx);
                }
                self.open_loading_editor_for_bound_insert(
                    &track_id,
                    &slot_id,
                    &log_display_name,
                    None,
                    cx,
                );
                let path = self
                    .timeline
                    .read(cx)
                    .state
                    .find_insert_slot(&track_id, &slot_id)
                    .and_then(|slot| slot.plugin_path.as_ref())
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let sample_rate = self.current_audio_sample_rate();
                let max_block_size = self
                    .audio_bridge
                    .engine
                    .as_ref()
                    .map(|engine| engine.config().buffer_size)
                    .unwrap_or(256);
                let descriptor = super::plugin_bridge_runtime::BridgePluginDescriptor {
                    track_id: track_id.clone(),
                    insert_id: slot_id.clone(),
                    plugin_path: path.clone(),
                    class_id: bridge_class_id.clone(),
                    display_name: log_display_name.clone(),
                    format: Some(plugin_format.label().to_string()),
                };
                match super::plugin_bridge_runtime::PluginBridgeRuntime::ensure_shared(
                    &mut self.plugin_editors.bridge_runtime,
                ) {
                    Ok(runtime) => {
                        let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                        let _ = self.timeline.update(cx, |timeline, _cx| {
                            timeline.state.set_insert_runtime(
                                &track_id,
                                &slot_id,
                                PluginRuntimeBackend::ExternalBridge,
                                PluginRuntimeState::Loading,
                                host_pid,
                            )
                        });
                        // Stage 3b: after the shared audio region is established
                        // (inside send_load_plugin), install its realtime sink on
                        // the audio engine so plugin DSP output mixes into the
                        // master (gated by FUTUREBOARD_PLUGIN_BRIDGE_AUDIO).
                        let mut bridge_sink = None;
                        let mut load_dispatch_failed = false;
                        match runtime.lock() {
                            Ok(mut runtime) => {
                                let load_result = if is_builtin {
                                    // Re-adding an insert whose state mirror is
                                    // still alive (e.g. host respawn): restore it.
                                    let state_json =
                                        crate::components::builtin_plugin_editor::builtin_state_bytes(
                                            &bridge_class_id,
                                            &slot_id,
                                        )
                                        .and_then(|bytes| String::from_utf8(bytes).ok());
                                    runtime.send_load_builtin_plugin(
                                        descriptor,
                                        sample_rate,
                                        max_block_size,
                                        state_json,
                                    )
                                } else if is_audio_unit {
                                    // A freshly added insert has no persisted
                                    // state; the unit opens at its defaults.
                                    runtime.send_load_au_plugin(
                                        descriptor,
                                        sample_rate,
                                        max_block_size,
                                        None,
                                    )
                                } else {
                                    runtime.send_load_plugin(
                                        descriptor,
                                        sample_rate,
                                        max_block_size,
                                    )
                                };
                                if let Err(error) = load_result {
                                    eprintln!(
                                        "[plugin-runtime] external bridge LoadPlugin failed: {error}"
                                    );
                                    load_dispatch_failed = true;
                                    let _ = self.timeline.update(cx, |timeline, _cx| {
                                        timeline.state.set_insert_runtime(
                                            &track_id,
                                            &slot_id,
                                            PluginRuntimeBackend::ExternalBridge,
                                            PluginRuntimeState::Failed(error.to_string()),
                                            host_pid,
                                        )
                                    });
                                } else {
                                    bridge_sink = runtime.audio_sink_for(&slot_id);
                                }
                            }
                            Err(_) => {
                                eprintln!("[plugin-runtime] external bridge runtime lock poisoned");
                                load_dispatch_failed = true;
                            }
                        }
                        // The request never reached the host, so no PluginLoaded
                        // / PluginLoadFailed will ever arrive to take the dialog
                        // down. Close it here instead of leaving it spinning.
                        if load_dispatch_failed {
                            self.end_plugin_load_progress(&slot_id, cx);
                        }
                        crate::forensic_trace::log_trace_plugin(&track_id, &slot_id);
                        let timeline_state = self.timeline.read(cx).state.clone();
                        crate::forensic_trace::log_plugin_main_registry(&timeline_state);
                        #[cfg(feature = "plugin-host-bin")]
                        SpherePluginHost::plugin_host_preview::PluginHostPreviewEngine::log_unified_runtime(
                            &track_id,
                            &slot_id,
                            &slot_id,
                        );
                        let _ = bridge_sink;
                        self.sync_plugin_bridge_sinks_to_engine(cx, "bridge_plugin_loaded");
                        if let Some(engine) = self.audio_bridge.engine.as_ref() {
                            eprintln!(
                                "[PluginAdd] was_playing={} state_before={:?} source=bridge_plugin_loaded",
                                engine.transport_playing(),
                                engine.engine_state(),
                            );
                        }
                        self.audio_bridge.project_dirty = true;
                        self.schedule_audio_project_sync(cx, true, "bridge_plugin_loaded");
                        self.mark_dirty();
                    }
                    Err(error) => {
                        eprintln!(
                            "[plugin-runtime] refusing in-process fallback while bridge is enabled"
                        );
                        self.end_plugin_load_progress(&slot_id, cx);
                        let _ = self.timeline.update(cx, |timeline, _cx| {
                            timeline.state.set_insert_runtime(
                                &track_id,
                                &slot_id,
                                PluginRuntimeBackend::ExternalBridge,
                                PluginRuntimeState::Failed(error.to_string()),
                                None,
                            )
                        });
                    }
                }
            } else {
                eprintln!(
                    "[plugin-runtime] backend=in_process reason=FUTUREBOARD_PLUGIN_LEGACY_IN_PROCESS=1"
                );
                eprintln!("[plugin-runtime] WARNING using legacy in-process plugin runtime");
                eprintln!(
                    "[plugin-runtime] legacy path may hang GPU/browser-backed plugin editors"
                );
                self.mark_dirty();
                self.audio_bridge.project_dirty = true;
            }
            if std::env::var_os("FUTUREBOARD_INSPECTOR_DEBUG").is_some()
                || std::env::var_os("FUTUREBOARD_PLUGIN_INSERT_DEBUG").is_some()
            {
                let kind = match desired_kind {
                    PluginInsertKind::Instrument => "Instrument",
                    PluginInsertKind::Effect => "Effect",
                };
                eprintln!(
                    "[inspector] insert apply track={} slot={} kind={} plugin={}",
                    track_id, target_slot_index, kind, log_display_name
                );
            }
            if plugin_id != STUB_PLUGIN_ID {
                self.plugin_picker_prefs.record_recent(plugin_id);
            }
            self.record_insert_chains(history_label, history_before, cx);
            opened_slot = Some((track_id.clone(), target_slot_index, slot_id));
        }
        self.plugin_picker = PluginPickerState::closed();
        cx.notify();
        opened_slot
    }

    pub(crate) fn set_insert_parameter_from_ui(
        &mut self,
        track_id: String,
        insert_id: String,
        param_id: u32,
        value: f32,
        cx: &mut Context<Self>,
    ) {
        let value = value.clamp(0.0, 1.0);
        let changed = self.timeline.update(cx, |timeline, cx| {
            let changed = timeline
                .state
                .set_insert_parameter_value(&track_id, &insert_id, param_id, value);
            if changed {
                cx.notify();
            }
            changed
        });
        eprintln!(
            "[PluginParam] parameter changed track={} insert={} param={} value={:.4}",
            track_id, insert_id, param_id, value
        );
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            if let Err(error) = engine.set_insert_param(
                track_id.clone(),
                insert_id.clone(),
                param_id.to_string(),
                value,
            ) {
                eprintln!(
                    "[PluginParam] set_parameter failed insert={} param={} error={}",
                    insert_id, param_id, error
                );
            }
        }
        if changed {
            self.mark_dirty();
        }
        cx.notify();
    }

    /// Apply a plug-in dropped from the Browser onto the arrangement.
    ///
    /// `target` comes from [`crate::components::timeline::PluginDropTarget`]'s
    /// resolver, the same one that drew the drag-over hint: onto an existing
    /// track (a header drop), or onto a new track at a given position (a
    /// timeline drop) — MIDI for an instrument, audio for an effect.
    pub(super) fn apply_dropped_plugin_drag(
        &mut self,
        plugin_id: &str,
        target: &crate::components::timeline::PluginDropTarget,
        kind: SpherePluginHost::PluginKind,
        cx: &mut Context<Self>,
    ) {
        use crate::components::plugin_picker::{PluginInsertKind, PluginPickerState};
        use crate::components::timeline::timeline_state::TrackType;
        use crate::components::timeline::{NewTrackKind, PluginDropTarget};

        let Some(plugin) = self
            .plugin_catalog
            .available
            .as_ref()
            .and_then(|plugins| plugins.iter().find(|plugin| plugin.id == plugin_id))
            .cloned()
        else {
            eprintln!("[PluginDrop] drag rejected: plugin not in catalog id={plugin_id}");
            return;
        };
        let desired_kind = match kind {
            SpherePluginHost::PluginKind::Instrument => PluginInsertKind::Instrument,
            _ => PluginInsertKind::Effect,
        };
        let track_id = match target {
            PluginDropTarget::Track { track_id } => track_id.clone(),
            PluginDropTarget::NewTrack {
                insert_index,
                kind: new_kind,
            } => {
                let (track_type, prefix) = match new_kind {
                    NewTrackKind::Midi => (TrackType::Midi, "MIDI"),
                    NewTrackKind::Audio => (TrackType::Audio, "Audio"),
                };
                let name = format!("{prefix} — {}", plugin.name);
                // Created at the end, then moved to where the drop said —
                // below the track the plug-in was released over.
                self.create_track_for_plugin_drop(track_type, name, Some(*insert_index), cx)
            }
            PluginDropTarget::Refused { .. } => return,
        };
        let (track_name, track_type) = self
            .timeline
            .read(cx)
            .state
            .find_track(&track_id)
            .map(|track| (track.name.clone(), track.track_type))
            .unwrap_or_else(|| ("MIDI Track".to_string(), TrackType::Midi));
        let next_slot_index = self
            .timeline
            .read(cx)
            .state
            .insert_slots(&track_id)
            .map(|slots| slots.len())
            .unwrap_or(0);
        // The real track type: the picker's validation decides from it, and
        // pretending every target was a MIDI track refused every effect
        // dropped on a header.
        self.plugin_picker = PluginPickerState::open_for_with_filter(
            &track_id,
            &track_name,
            track_type,
            next_slot_index,
            false,
            crate::components::plugin_picker::PickerFilter::All,
            desired_kind,
        );
        let _ = self.apply_picked_insert(plugin_id, cx);
    }

    /// A plug-in preset dropped on the arrangement. `track_id` is the track it
    /// landed on, or `None` for the empty space below the last track.
    ///
    /// An instrument always gets its own new MIDI track (a second instrument
    /// on an existing track would silently replace the first). An effect goes
    /// onto the track it was dropped on, or onto a new audio track when it
    /// was dropped on empty space.
    pub(super) fn apply_dropped_plugin_preset(
        &mut self,
        track_id: Option<&str>,
        preset_path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Option<(String, usize, String)> {
        use crate::components::timeline::timeline_state::TrackType;
        use SpherePluginHost::PluginKind;

        let reg = self.read_dropped_plugin_preset(preset_path)?;
        if reg.kind == PluginKind::Instrument {
            return self.create_instrument_track_from_preset(&reg, cx);
        }
        let track_id = match track_id {
            Some(track_id) => track_id.to_string(),
            None => self.create_track_for_plugin_drop(
                TrackType::Audio,
                format!("Audio — {}", reg.name),
                None,
                cx,
            ),
        };
        let slot_index = self
            .timeline
            .read(cx)
            .state
            .insert_slots(&track_id)
            .map(|slots| slots.len())
            .unwrap_or(0);
        self.bind_preset_to_insert_slot(&track_id, slot_index, &reg, cx, "plugin_preset_drop")
    }

    /// A fresh track for a plug-in dropped where no track was, selected, and
    /// moved to `insert_index` when one is given (it is created at the end).
    fn create_track_for_plugin_drop(
        &mut self,
        track_type: crate::components::timeline::timeline_state::TrackType,
        name: String,
        insert_index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> String {
        use crate::components::timeline::timeline_state::{
            CreateTrackOptions, InputMonitorMode, TrackEditScope,
        };
        self.timeline.update(cx, |timeline, cx| {
            // Its own step, before the plug-in that goes on it (which records
            // one of its own): undo takes the plug-in off, then the track.
            let edit = timeline.begin_track_edit(TrackEditScope::track_list());
            let id = timeline.state.create_track(CreateTrackOptions {
                track_type,
                name,
                color: timeline
                    .state
                    .track_color_for_index(timeline.state.tracks.len()),
                volume: crate::components::timeline::timeline_state::volume::db_to_norm(0.0),
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            if let Some(insert_index) = insert_index {
                if insert_index < timeline.state.tracks.len() - 1 {
                    timeline.state.reorder_track(&id, insert_index);
                }
            }
            timeline.state.select_track(&id);
            timeline.commit_track_edit("Add Track", edit, false, cx);
            cx.notify();
            id
        })
    }

    pub(super) fn apply_dropped_plugin_preset_to_slot(
        &mut self,
        track_id: &str,
        slot_index: usize,
        preset_path: &std::path::Path,
        cx: &mut Context<Self>,
    ) -> Option<(String, usize, String)> {
        use SpherePluginHost::PluginKind;

        let reg = self.read_dropped_plugin_preset(preset_path)?;
        if reg.kind == PluginKind::Instrument {
            eprintln!(
                "[PluginDrop] ignored instrument preset on effect slot preset={} plugin={}",
                preset_path.display(),
                reg.name
            );
            return None;
        }
        self.bind_preset_to_insert_slot(track_id, slot_index, &reg, cx, "mixer_preset_drop")
    }

    fn read_dropped_plugin_preset(
        &self,
        preset_path: &std::path::Path,
    ) -> Option<SpherePluginHost::RegistryPlugin> {
        let reg = match SpherePluginHost::preset::read_preset_file(preset_path) {
            Ok(reg) => reg,
            Err(error) => {
                eprintln!(
                    "[PluginDrop] failed to read preset path={} error={error}",
                    preset_path.display()
                );
                return None;
            }
        };
        if !reg.supports_insert() {
            eprintln!(
                "[PluginDrop] unsupported preset name={} format={:?} status={:?}",
                reg.name, reg.format, reg.status
            );
            return None;
        }
        Some(reg)
    }

    fn bind_preset_to_insert_slot(
        &mut self,
        track_id: &str,
        slot_index: usize,
        reg: &SpherePluginHost::RegistryPlugin,
        cx: &mut Context<Self>,
        source: &'static str,
    ) -> Option<(String, usize, String)> {
        use crate::components::timeline::timeline_state::TrackType;

        let can_host_effect = {
            let timeline = self.timeline.read(cx);
            if track_id == crate::components::timeline::timeline_state::MASTER_TRACK_ID {
                true
            } else {
                timeline
                    .state
                    .tracks
                    .iter()
                    .find(|track| track.id == track_id)
                    .is_some_and(|track| track.track_type != TrackType::Midi)
            }
        };
        if !can_host_effect {
            eprintln!("[PluginDrop] effect preset cannot be inserted on track={track_id}");
            return None;
        }

        let (plugin_id, plugin_path, plugin_format, vendor, display_name) =
            Self::registry_insert_descriptor(reg);
        let existing_slot_id = self
            .timeline
            .read(cx)
            .state
            .insert_slot_at(track_id, slot_index)
            .filter(|slot| !slot.is_empty())
            .map(|slot| slot.id.clone());
        // One undo step, as with the picker; a replaced plug-in comes back as
        // it sounds now.
        let history_label = if existing_slot_id.is_some() {
            "Replace Plug-in"
        } else {
            "Add Plug-in"
        };
        if let Some(old_slot_id) = existing_slot_id.as_ref() {
            self.store_live_plugin_states(track_id, std::slice::from_ref(old_slot_id), cx);
        }
        let history_before = self.capture_insert_chains(&[track_id], cx);
        let slot_id = if let Some(old_slot_id) = existing_slot_id {
            self.teardown_insert_instance(track_id, &old_slot_id, cx, source);
            self.timeline.update(cx, |timeline, _cx| {
                timeline
                    .state
                    .replace_insert_with_fresh_slot(track_id, &old_slot_id)
            })
        } else {
            self.timeline.update(cx, |timeline, _cx| {
                timeline.state.ensure_insert_slot_at(track_id, slot_index)
            })
        }?;

        self.close_insert_editor(track_id, &slot_id, cx);
        self.timeline.update(cx, |timeline, _cx| {
            timeline.state.set_insert_plugin(
                track_id,
                &slot_id,
                plugin_id,
                Some(plugin_path),
                plugin_format,
                vendor.clone(),
                display_name.clone(),
            );
            timeline
                .state
                .set_insert_plugin_role(track_id, &slot_id, false);
        });

        eprintln!(
            "[PluginDrop] track={track_id} slot={slot_id} index={slot_index} plugin={}",
            display_name
        );
        self.record_insert_chains(history_label, history_before, cx);
        self.after_preset_insert_bound(track_id, &slot_id, plugin_format, cx, source);
        cx.notify();
        Some((track_id.to_string(), slot_index, slot_id))
    }

    fn create_instrument_track_from_preset(
        &mut self,
        reg: &SpherePluginHost::RegistryPlugin,
        cx: &mut Context<Self>,
    ) -> Option<(String, usize, String)> {
        use crate::components::timeline::timeline_state::TrackType;

        let (plugin_id, plugin_path, plugin_format, vendor, display_name) =
            Self::registry_insert_descriptor(reg);
        // The track and its instrument are one step.
        let edit = self.begin_track_edit(
            crate::components::timeline::timeline_state::TrackEditScope::track_list(),
            cx,
        );
        let created = self.timeline.update(cx, |timeline, _cx| {
            // A dropped instrument preset must have a MIDI-producing track
            // before the plugin instance is created. Creating an Instrument
            // track directly used to race the bridge's MIDI route setup: the
            // plugin existed, but no MIDI source was available when its first
            // state/preview request was dispatched. A MIDI track is also a
            // valid instrument host in the runtime (`bridge_instrument_instance_id`),
            // and keeps the stable track/insert ownership in one operation.
            let track_id = timeline.state.create_track(
                crate::components::timeline::timeline_state::CreateTrackOptions {
                    track_type: TrackType::Midi,
                    name: format!("MIDI — {display_name}"),
                    color: timeline
                        .state
                        .track_color_for_index(timeline.state.tracks.len()),
                    volume: crate::components::timeline::timeline_state::volume::db_to_norm(0.0),
                    pan: 0.0,
                    armed: false,
                    input_monitor:
                        crate::components::timeline::timeline_state::InputMonitorMode::Off,
                },
            );
            let slot_id = timeline.state.add_insert(&track_id)?;
            timeline.state.set_insert_plugin(
                &track_id,
                &slot_id,
                plugin_id,
                Some(plugin_path),
                plugin_format,
                vendor.clone(),
                display_name.clone(),
            );
            timeline
                .state
                .set_insert_plugin_role(&track_id, &slot_id, true);
            timeline.state.select_track(&track_id);
            Some((track_id, 0usize, slot_id))
        })?;
        self.commit_track_edit("Add Instrument Track", edit, cx);

        eprintln!(
            "[PluginDrop] midi track created before instrument plugin track={} slot={} plugin={}",
            created.0, created.2, display_name
        );
        self.after_preset_insert_bound(
            &created.0,
            &created.2,
            plugin_format,
            cx,
            "instrument_preset_drop",
        );
        cx.notify();
        Some(created)
    }

    fn after_preset_insert_bound(
        &mut self,
        track_id: &str,
        slot_id: &str,
        plugin_format: crate::components::timeline::timeline_state::InsertPluginFormat,
        cx: &mut Context<Self>,
        source: &'static str,
    ) {
        use crate::components::timeline::timeline_state::InsertPluginFormat;
        if matches!(
            plugin_format,
            InsertPluginFormat::Vst3
                | InsertPluginFormat::Vst2
                | InsertPluginFormat::Clap
                | InsertPluginFormat::Au
        ) && super::plugin_bridge_runtime::bridge_enabled()
        {
            let display_name = self
                .timeline
                .read(cx)
                .state
                .find_insert_slot(track_id, slot_id)
                .map(|slot| slot.display_name.clone())
                .unwrap_or_else(|| "Plugin".to_string());
            // Built-ins load instantly (see `apply_plugin_pick`) — no dialog.
            let is_builtin = self
                .timeline
                .read(cx)
                .state
                .find_insert_slot(track_id, slot_id)
                .and_then(|slot| slot.plugin_id.clone())
                .is_some_and(|id| SpherePluginHost::builtin_audio_bridge_supported(&id));
            if !is_builtin {
                self.begin_plugin_load_progress(slot_id, &display_name, cx);
            }
            self.open_loading_editor_for_bound_insert(track_id, slot_id, &display_name, None, cx);
            if self.load_bridge_insert_for_slot(track_id, slot_id, cx) {
                return;
            }
            // The load was never dispatched, so no host event will close the
            // dialog.
            self.end_plugin_load_progress(slot_id, cx);
        }
        self.mark_dirty();
        self.audio_bridge.project_dirty = true;
        self.schedule_audio_project_sync(cx, true, source);
    }

    fn registry_insert_descriptor(
        reg: &SpherePluginHost::RegistryPlugin,
    ) -> (
        String,
        std::path::PathBuf,
        crate::components::timeline::timeline_state::InsertPluginFormat,
        Option<String>,
        String,
    ) {
        use crate::components::timeline::timeline_state::InsertPluginFormat;
        use SpherePluginHost::PluginFormat as RegFmt;

        let plugin_format = match reg.format {
            RegFmt::Vst3 => InsertPluginFormat::Vst3,
            RegFmt::Vst2 => InsertPluginFormat::Vst2,
            RegFmt::Clap => InsertPluginFormat::Clap,
            RegFmt::Au => InsertPluginFormat::Au,
            RegFmt::Lv2 => InsertPluginFormat::Lv2,
            _ => InsertPluginFormat::Unknown,
        };
        (
            reg.class_id.clone().unwrap_or_else(|| reg.id.clone()),
            reg.path.clone(),
            plugin_format,
            Some(reg.vendor.clone()).filter(|vendor| !vendor.trim().is_empty()),
            reg.name.clone(),
        )
    }

    pub(super) fn flush_deferred_insert_editor_opens(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.plugin_editors.deferred_opens.is_empty() {
            // Nothing queued this frame: every instance has resolved, so clear
            // the loop-guard counters (they only track *consecutive* re-queues).
            if !self.plugin_editors.flush_attempts.is_empty() {
                self.plugin_editors.flush_attempts.clear();
            }
            return;
        }
        // One automatic attempt per readiness is the intended cap (spec item 12);
        // the deferred queue is normally drained once. The guard exists only to
        // make an unexpected re-queue source terminate instead of spinning.
        const MAX_EDITOR_OPEN_FLUSHES: u32 = 10;
        let pending: Vec<_> = self.plugin_editors.deferred_opens.drain(..).collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (track_id, insert_index, instance_id) in pending {
            seen.insert(instance_id.clone());
            let attempts = self
                .plugin_editors
                .flush_attempts
                .entry(instance_id.clone())
                .or_insert(0);
            *attempts += 1;
            if *attempts > MAX_EDITOR_OPEN_FLUSHES {
                let count = *attempts;
                eprintln!(
                    "[EDITOR_LOOP_GUARD]\nplugin_instance_id={instance_id}\nevent_name=flush_open\nsame_state_repeat_count={count}\nlast_transition_age_ms=unknown\nERROR=true"
                );
                // Force the editor open terminal so it stops re-queueing and the
                // user can retry deliberately; the plugin/audio instance is left
                // untouched.
                self.timeline.update(cx, |timeline, _cx| {
                    for track in timeline.state.insert_owner_ids_containing(&instance_id) {
                        timeline
                            .state
                            .set_insert_pending_editor_open(&track, &instance_id, false);
                    }
                });
                for session in self.plugin_editors.bridge.values_mut() {
                    if session.instance_id == instance_id
                        && !bridge_editor_is_terminal(&session.state)
                    {
                        session.shell.set_status(
                            "Editor open loop detected — close and open it again.",
                            true,
                        );
                        transition_bridge_editor_state(
                            session,
                            BridgeEditorState::TimedOut("Editor open loop guard".to_string()),
                            "loop_guard",
                        );
                    }
                }
                continue;
            }
            eprintln!(
                "[EDITOR_QUEUE_FLUSH]\nplugin_instance_id={instance_id}\nflush_attempt={}\nsent_open_editor=true",
                *attempts
            );
            let resolved = {
                let timeline = self.timeline.read(cx);
                timeline
                    .state
                    .insert_slots(&track_id)
                    .and_then(|slots| {
                        slots
                            .iter()
                            .enumerate()
                            .find(|(_, slot)| slot.id == instance_id)
                            .map(|(index, _)| (track_id.clone(), index))
                    })
                    .or_else(|| {
                        timeline.state.tracks.iter().find_map(|track| {
                            track
                                .inserts
                                .iter()
                                .enumerate()
                                .find(|(_, slot)| slot.id == instance_id)
                                .map(|(index, _)| (track.id.clone(), index))
                        })
                    })
                    .or_else(|| {
                        timeline
                            .state
                            .master
                            .inserts
                            .iter()
                            .enumerate()
                            .find(|(_, slot)| slot.id == instance_id)
                            .map(|(index, _)| {
                                (
                                    crate::components::timeline::timeline_state::MASTER_TRACK_ID
                                        .to_string(),
                                    index,
                                )
                            })
                    })
            };
            let Some((resolved_track_id, resolved_index)) = resolved else {
                eprintln!(
                    "[EDITOR_QUEUE_FLUSH]\nplugin_instance_id={instance_id}\nreason=instance_not_found action=drop"
                );
                continue;
            };
            if resolved_track_id != track_id || resolved_index != insert_index {
                eprintln!(
                    "[EDITOR_QUEUE_FLUSH]\nplugin_instance_id={instance_id}\nstale_track_id={track_id}\nstale_slot_index={insert_index}\nresolved_track_id={resolved_track_id}\nresolved_slot_index={resolved_index}"
                );
            }
            self.open_insert_editor(&resolved_track_id, resolved_index, &instance_id, window, cx);
        }
        // Drop counters for instances that were not re-queued this frame.
        self.plugin_editors
            .flush_attempts
            .retain(|id, _| seen.contains(id));
    }

    /// All (track_id, insert_id) pairs whose insert is hosted by the plug-in
    /// host process over the shared-audio bridge.
    fn bridge_hosted_insert_slots(&self, cx: &App) -> Vec<(String, String)> {
        if !super::plugin_bridge_runtime::bridge_enabled() {
            return Vec::new();
        }
        let timeline = self.timeline.read(cx);
        let state = &timeline.state;
        let mut slots: Vec<(String, String)> = state
            .tracks
            .iter()
            .flat_map(|track| {
                track
                    .inserts
                    .iter()
                    .filter(|slot| slot.is_bridge_hosted_external_module())
                    .map(|slot| (track.id.clone(), slot.id.clone()))
            })
            .collect();
        slots.extend(
            state
                .master
                .inserts
                .iter()
                .filter(|slot| slot.is_bridge_hosted_external_module())
                .map(|slot| {
                    (
                        crate::components::timeline::timeline_state::MASTER_TRACK_ID.to_string(),
                        slot.id.clone(),
                    )
                }),
        );
        slots
    }

    /// All inserts whose audio is rendered through the shared-memory bridge,
    /// including Futureboard built-ins that have no module path.
    fn bridge_audio_insert_slots(&self, cx: &App) -> Vec<(String, String)> {
        let mut slots = self.bridge_hosted_insert_slots(cx);
        if !super::plugin_bridge_runtime::bridge_enabled() {
            return slots;
        }
        let state = &self.timeline.read(cx).state;
        slots.extend(state.tracks.iter().flat_map(|track| {
            track
                .inserts
                .iter()
                .filter(|slot| {
                    slot.plugin_id
                        .as_deref()
                        .is_some_and(SpherePluginHost::builtin_audio_bridge_supported)
                })
                .map(|slot| (track.id.clone(), slot.id.clone()))
        }));
        slots.extend(state.master.inserts.iter().filter_map(|slot| {
            slot.plugin_id
                .as_deref()
                .filter(|id| SpherePluginHost::builtin_audio_bridge_supported(id))
                .map(|_| {
                    (
                        crate::components::timeline::timeline_state::MASTER_TRACK_ID.to_string(),
                        slot.id.clone(),
                    )
                })
        }));
        slots
    }

    /// One insert's opaque state as the plug-in has it now, for a copy of it.
    ///
    /// A built-in's comes from the main-process mirror, which the editor keeps
    /// current. A bridged plug-in is asked for it, as a preset save does: the
    /// slot's stored copy is only as new as the last save. The stored copy is
    /// the answer when neither has anything newer. Bounded by the bridge's
    /// state request timeout — a user gesture, never per frame.
    pub(super) fn capture_insert_state(
        &mut self,
        track_id: &str,
        insert_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<std::sync::Arc<Vec<u8>>> {
        self.capture_insert_states(track_id, std::slice::from_ref(&insert_id.to_string()), cx)
            .remove(insert_id)
    }

    /// [`Self::capture_insert_state`] for several of one channel's inserts,
    /// keyed by insert id; an insert with no state has no entry. Every bridged
    /// one is asked in a single request, so a whole chain costs one timeout at
    /// most, not one per plug-in.
    pub(super) fn capture_insert_states(
        &mut self,
        track_id: &str,
        insert_ids: &[String],
        cx: &mut Context<Self>,
    ) -> std::collections::HashMap<String, std::sync::Arc<Vec<u8>>> {
        let slots: Vec<_> = {
            let state = &self.timeline.read(cx).state;
            insert_ids
                .iter()
                .filter_map(|id| state.find_insert_slot(track_id, id).cloned())
                .filter(|slot| !slot.is_empty())
                .collect()
        };
        let mut states = std::collections::HashMap::new();
        let mut ask_bridge = Vec::new();
        for slot in &slots {
            let plugin_id = slot.plugin_id.as_deref().unwrap_or_default();
            if SpherePluginHost::builtin_audio_bridge_supported(plugin_id) {
                if let Some(bytes) = crate::components::builtin_plugin_editor::builtin_state_bytes(
                    plugin_id, &slot.id,
                ) {
                    states.insert(slot.id.clone(), std::sync::Arc::new(bytes));
                }
            } else if slot.runtime_backend == PluginRuntimeBackend::ExternalBridge {
                ask_bridge.push(slot.id.clone());
            }
        }
        if !ask_bridge.is_empty() {
            if let Some(runtime) = self.plugin_editors.bridge_runtime.clone() {
                if let Ok(mut runtime) = runtime.lock() {
                    let capture = runtime
                        .request_plugin_states(&ask_bridge, std::time::Duration::from_millis(1500));
                    for (id, bytes) in capture.states {
                        if !bytes.is_empty() {
                            states.insert(id, std::sync::Arc::new(bytes));
                        }
                    }
                }
            }
        }
        for slot in slots {
            if states.contains_key(&slot.id) {
                continue;
            }
            eprintln!(
                "[plugin-state] no live state from insert={}; using the stored copy (present={})",
                slot.id,
                slot.vst3_state.is_some()
            );
            if let Some(stored) = slot.vst3_state {
                states.insert(slot.id, stored);
            }
        }
        states
    }

    /// Load inserts the model just added as copies — a duplicated insert, a
    /// cloned track's plug-ins, a pasted chain — each already carrying its
    /// state in `vst3_state`. A built-in's mirror is seeded first, since its
    /// editor and its replay read that rather than the slot. The caller
    /// schedules the engine sync that an in-process insert loads through.
    pub(super) fn load_copied_inserts(
        &mut self,
        track_id: &str,
        insert_ids: &[String],
        cx: &mut Context<Self>,
    ) {
        for insert_id in insert_ids {
            let seed = self
                .timeline
                .read(cx)
                .state
                .find_insert_slot(track_id, insert_id)
                .and_then(|slot| Some((slot.plugin_id.clone()?, slot.vst3_state.clone()?)));
            let Some((plugin_id, state)) = seed else {
                // Empty, or nothing captured: it loads at its defaults.
                self.load_bridge_insert_for_slot(track_id, insert_id, cx);
                continue;
            };
            crate::components::builtin_plugin_editor::builtin_state_seed(
                &plugin_id, insert_id, &state,
            );
            self.load_bridge_insert_for_slot(track_id, insert_id, cx);
        }
    }

    /// Pull current VST3 states from the plugin host into the timeline slots
    /// (`InsertSlotState::vst3_state`) so the next project snapshot persists
    /// them. Bounded request/response — call on save, not per frame. Slots the
    /// host did not answer for keep their previously captured state; `capture`
    /// says what the states are for, which decides how that is reported.
    pub(super) fn refresh_bridge_plugin_states(
        &mut self,
        capture: PluginStateCaptureFor,
        cx: &mut Context<Self>,
    ) {
        // Built-in inserts: flush the main-process state mirror into each
        // slot's persisted blob so the imminent save writes real state (the
        // mirror is authoritative; nothing is fetched from the host).
        self.timeline.update(cx, |timeline, _cx| {
            let flush =
                |slot: &mut crate::components::timeline::timeline_state::InsertSlotState| {
                    let Some(plugin_id) = slot.plugin_id.as_deref() else {
                        return;
                    };
                    if !SpherePluginHost::builtin_audio_bridge_supported(plugin_id) {
                        return;
                    }
                    if let Some(bytes) =
                        crate::components::builtin_plugin_editor::builtin_state_bytes(
                            plugin_id, &slot.id,
                        )
                    {
                        slot.vst3_state = Some(std::sync::Arc::new(bytes));
                    }
                };
            for slot in &mut timeline.state.master.inserts {
                flush(slot);
            }
            for track in &mut timeline.state.tracks {
                for slot in &mut track.inserts {
                    flush(slot);
                }
            }
        });

        let slots = self.bridge_hosted_insert_slots(cx);
        if slots.is_empty() {
            self.report_uncaptured_plugin_states(&[], capture, cx);
            return;
        }
        let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() else {
            return;
        };
        let instance_ids: Vec<String> = slots.iter().map(|(_, id)| id.clone()).collect();
        let captured = match runtime.lock() {
            Ok(mut runtime) => {
                runtime.request_plugin_states(&instance_ids, std::time::Duration::from_millis(1500))
            }
            Err(_) => {
                eprintln!("[plugin-bridge] state refresh skipped: runtime lock poisoned");
                return;
            }
        };
        self.report_uncaptured_plugin_states(&captured.unanswered, capture, cx);
        let states = captured.states;
        if states.is_empty() {
            return;
        }
        eprintln!(
            "[plugin-bridge] refreshed plugin states for save captured={} of {}",
            states.len(),
            instance_ids.len()
        );
        self.timeline.update(cx, |timeline, _cx| {
            for slot in &mut timeline.state.master.inserts {
                if let Some(packed) = states.get(&slot.id) {
                    slot.vst3_state = Some(std::sync::Arc::new(packed.clone()));
                }
            }
            for track in &mut timeline.state.tracks {
                for slot in &mut track.inserts {
                    if let Some(packed) = states.get(&slot.id) {
                        slot.vst3_state = Some(std::sync::Arc::new(packed.clone()));
                    }
                }
            }
        });
    }

    /// Say which plug-ins' state a capture could not get in time, and after a
    /// plain save keep the project unsaved.
    ///
    /// Their slots keep the state captured before, so a file written now may
    /// hold older settings than the plug-in has. Shown as a failed background
    /// job naming them, worded for what `capture` is for (the status bar turns
    /// to an error until a later capture gets everything). After a plain save
    /// a session that was dirty is marked dirty again — deferred, so the mark
    /// lands after the snapshot and the finished save cannot clear it. Never
    /// on a save that goes on to close or switch the project, which would
    /// then ask "Save changes?" again for as long as the plug-in does not
    /// answer (see [`PluginStateCaptureFor::keeps_session_dirty`]). A clean
    /// session stays clean: nothing was edited that the file lacks.
    fn report_uncaptured_plugin_states(
        &mut self,
        unanswered: &[String],
        capture: PluginStateCaptureFor,
        cx: &mut Context<Self>,
    ) {
        const TASK_ID: &str = PLUGIN_STATE_CAPTURE_TASK_ID;
        if unanswered.is_empty() {
            let failed = self
                .background_tasks
                .tasks
                .get(TASK_ID)
                .is_some_and(|task| task.status == crate::components::BackgroundTaskStatus::Failed);
            if failed {
                self.complete_background_task(
                    TASK_ID,
                    Some("Every plug-in's state was captured".to_string()),
                );
            }
            return;
        }
        let names: Vec<String> = {
            let state = &self.timeline.read(cx).state;
            unanswered
                .iter()
                .map(|instance_id| plugin_state_label(state, instance_id))
                .collect()
        };
        let message = uncaptured_plugin_states_message(&names, capture);
        eprintln!("[plugin-bridge] {message}");
        self.start_background_task(
            TASK_ID,
            crate::components::BackgroundTaskKind::ProjectSave,
            "Plug-in state not captured",
            None,
            None,
            false,
        );
        self.fail_background_task(TASK_ID, message);
        cx.notify();
        if !capture.keeps_session_dirty(self.project_session.is_dirty) {
            return;
        }
        let session = self.session_generation();
        let studio = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = studio.update(cx, |layout, cx| {
                if layout.session_generation() == session {
                    layout.mark_dirty_view_only();
                    cx.notify();
                }
            });
        });
    }

    /// Install one realtime bridge sink per insert instance (independent
    /// request_seq/done_seq for serial FX chains). Idempotent.
    pub(super) fn sync_plugin_bridge_sinks_to_engine(
        &mut self,
        cx: &mut Context<Self>,
        reason: &'static str,
    ) -> bool {
        let slots = self.bridge_audio_insert_slots(cx);
        if slots.is_empty() {
            return false;
        }
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            eprintln!(
                "[PluginRestore] bridge sink deferred reason=no_audio_engine source={reason}"
            );
            return false;
        };
        let Some(runtime_arc) = self.plugin_editors.bridge_runtime.as_ref() else {
            return false;
        };
        let Ok(runtime) = runtime_arc.lock() else {
            return false;
        };
        let mut installed = false;
        for (track_id, insert_id) in &slots {
            let Some(sink) = runtime.audio_sink_for(insert_id) else {
                eprintln!(
                    "[PluginRestore] bridge sink deferred instance={insert_id} reason=no_shared_audio source={reason}"
                );
                continue;
            };
            let region_name = super::plugin_bridge_runtime::bridge_region_name(insert_id);
            eprintln!(
                "[PluginAdd] bridge_key={insert_id} shared_region={region_name} track={track_id}"
            );
            match engine.set_plugin_bridge_sink(insert_id.clone(), Some(sink)) {
                Ok(()) => {
                    eprintln!(
                        "[PluginRestore] bridge registered instance={insert_id} track={track_id} source={reason}"
                    );
                    installed = true;
                }
                Err(error) => {
                    eprintln!(
                        "[plugin-bridge] engine set_plugin_bridge_sink failed instance={insert_id}: {error}"
                    );
                }
            }
        }
        installed
    }

    /// Host reported the insert instance is loaded (fresh or reused). Bind DSP
    /// into the audio engine and refresh the runtime graph snapshot.
    fn on_bridge_plugin_host_ready(
        &mut self,
        plugin_instance_id: &str,
        name: &str,
        runtime: &super::plugin_bridge_runtime::SharedPluginBridgeRuntime,
        cx: &mut Context<Self>,
        source: &'static str,
    ) -> bool {
        let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
        let mut pending_opens = Vec::new();
        let slot_changed = self.timeline.update(cx, |timeline, _cx| {
            let mut local_changed = false;
            let track_ids = timeline.state.insert_owner_ids_containing(plugin_instance_id);
            for track_id in &track_ids {
                eprintln!(
                    "[PluginRestore] graph insert bound track={track_id} slot={plugin_instance_id} instance_id={plugin_instance_id} plugin={name}"
                );
            }
            for track_id in track_ids {
                if timeline.state.set_insert_runtime(
                    &track_id,
                    plugin_instance_id,
                    PluginRuntimeBackend::ExternalBridge,
                    PluginRuntimeState::Ready,
                    host_pid,
                ) {
                    local_changed = true;
                }
                if let Some((index, true)) = timeline
                    .state
                    .insert_slots(&track_id)
                    .and_then(|slots| {
                        slots
                            .iter()
                            .enumerate()
                            .find(|(_, slot)| slot.id == plugin_instance_id)
                            .map(|(index, slot)| (index, slot.pending_open_editor))
                    })
                {
                    timeline.state.set_insert_pending_editor_open(
                        &track_id,
                        plugin_instance_id,
                        false,
                    );
                    pending_opens.push((track_id.clone(), index, plugin_instance_id.to_string()));
                }
            }
            local_changed
        });
        for open in pending_opens {
            if !self
                .plugin_editors
                .deferred_opens
                .iter()
                .any(|(_, _, id)| id == &open.2)
            {
                self.plugin_editors.deferred_opens.push(open);
            }
        }
        eprintln!("[plugin-runtime] state Loading -> Ready source={source}");
        self.sync_plugin_bridge_sinks_to_engine(cx, source);
        // Built-in insert: the host DSP always (re)starts at defaults —
        // replay the mirrored/persisted state through the live param channel
        // now that the sink is installed. Covers project open and host
        // crash/respawn.
        self.replay_builtin_insert_state(plugin_instance_id, cx);
        if slot_changed {
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, true, source);
        }
        self.request_bridge_insert_parameters(plugin_instance_id);
        slot_changed
    }

    /// Push a built-in insert's mirrored parameter state into its
    /// freshly-loaded host DSP as ordered wire-index param events (same
    /// channel as live editor edits: engine command → callback thread → SPSC
    /// ring → host producer). No-op for VST3 inserts, missing engine, or an
    /// insert with no mirrored/persisted state (host defaults already match).
    pub(super) fn replay_builtin_insert_state(&self, plugin_instance_id: &str, cx: &Context<Self>) {
        use crate::components::builtin_plugin_editor as host;
        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return;
        };
        let state = &self.timeline.read(cx).state;
        let master_id = crate::components::timeline::timeline_state::MASTER_TRACK_ID;
        let owners = std::iter::once((master_id, &state.master.inserts)).chain(
            state
                .tracks
                .iter()
                .map(|track| (track.id.as_str(), &track.inserts)),
        );
        for (track_id, inserts) in owners {
            for slot in inserts {
                if slot.id != plugin_instance_id {
                    continue;
                }
                let Some(plugin_id) = slot.plugin_id.as_deref() else {
                    return;
                };
                if !SpherePluginHost::builtin_audio_bridge_supported(plugin_id) {
                    return;
                }
                if let Some(blob) = slot.vst3_state.as_deref() {
                    host::builtin_state_seed(plugin_id, &slot.id, blob);
                }
                let values = host::builtin_state_replay(plugin_id, &slot.id);
                if values.is_empty() {
                    return;
                }
                eprintln!(
                    "[PluginRestore] builtin state replay instance={} params={}",
                    slot.id,
                    values.len()
                );
                for (index, value) in values {
                    if let Err(error) = engine.set_insert_param(
                        track_id.to_string(),
                        slot.id.clone(),
                        index.to_string(),
                        value,
                    ) {
                        eprintln!(
                            "[PluginRestore] builtin state replay failed instance={} index={index} error={error}",
                            slot.id
                        );
                        return;
                    }
                }
                return;
            }
        }
    }

    fn host_plugin_parameter_to_ui(
        param: &SpherePluginHost::ipc::HostPluginParameter,
    ) -> crate::components::timeline::timeline_state::PluginParameterState {
        use crate::components::timeline::timeline_state::PluginParameterState;
        let name = if !param.title.is_empty() {
            param.title.clone()
        } else {
            param.short_title.clone()
        };
        PluginParameterState {
            id: param.id,
            name,
            value_normalized: 0.5,
            automatable: param.automatable,
            hidden: param.hidden,
            read_only: param.read_only,
            unit: param.unit.clone(),
        }
    }

    pub(super) fn request_bridge_insert_parameters(&self, plugin_instance_id: &str) {
        let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() else {
            return;
        };
        if let Ok(mut bridge) = runtime.lock() {
            if let Err(error) = bridge.request_plugin_parameters(plugin_instance_id) {
                eprintln!(
                    "[plugin-bridge] GetPluginParameters send failed instance={plugin_instance_id}: {error}"
                );
            }
        }
    }

    pub(super) fn request_track_insert_parameters(&self, track_id: &str, cx: &Context<Self>) {
        let timeline = self.timeline.read(cx);
        let state = &timeline.state;
        let instance_ids: Vec<String> =
            if track_id == crate::components::timeline::timeline_state::MASTER_TRACK_ID {
                state
                    .master
                    .inserts
                    .iter()
                    .filter(|insert| !insert.is_empty())
                    .map(|insert| insert.id.clone())
                    .collect()
            } else {
                state
                    .find_track(track_id)
                    .map(|track| {
                        track
                            .inserts
                            .iter()
                            .filter(|insert| !insert.is_empty())
                            .map(|insert| insert.id.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            };
        for instance_id in instance_ids {
            self.request_bridge_insert_parameters(&instance_id);
        }
    }

    fn apply_bridge_insert_parameters(
        &mut self,
        plugin_instance_id: &str,
        parameters: Vec<SpherePluginHost::ipc::HostPluginParameter>,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::components::timeline::timeline_state::PluginParameterState;
        let ui_params: Vec<PluginParameterState> = parameters
            .iter()
            .map(Self::host_plugin_parameter_to_ui)
            .collect();
        self.timeline.update(cx, |timeline, _cx| {
            let track_ids = timeline
                .state
                .insert_owner_ids_containing(plugin_instance_id);
            track_ids.into_iter().any(|track_id| {
                timeline.state.set_insert_parameters(
                    &track_id,
                    plugin_instance_id,
                    ui_params.clone(),
                )
            })
        })
    }

    /// Load one external-bridge insert slot into the plugin host (shared by
    /// picker apply and project-load restore).
    pub(super) fn load_bridge_insert_for_slot(
        &mut self,
        track_id: &str,
        slot_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        use crate::components::timeline::timeline_state::{
            InsertPluginFormat, PluginRuntimeBackend, PluginRuntimeState,
        };

        if !super::plugin_bridge_runtime::bridge_enabled() {
            return false;
        }

        let slot = self
            .timeline
            .read(cx)
            .state
            .find_insert_slot(track_id, slot_id)
            .cloned();
        let Some(slot) = slot else {
            return false;
        };
        let class_id = slot.plugin_id.clone().unwrap_or_default();
        let is_builtin = SpherePluginHost::builtin_audio_bridge_supported(&class_id);
        let is_audio_unit = !is_builtin && slot.plugin_format == Some(InsertPluginFormat::Au);
        let is_module_plugin = matches!(
            slot.plugin_format,
            Some(InsertPluginFormat::Vst3 | InsertPluginFormat::Vst2 | InsertPluginFormat::Clap)
        );
        if !is_builtin && !is_audio_unit && !is_module_plugin {
            return false;
        }
        let path = slot.plugin_path.as_ref();
        let path_string = path
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let display_name = slot.display_name.clone();

        eprintln!(
            "[PluginRestore] project insert found track={track_id} slot={slot_id} plugin={display_name}"
        );
        eprintln!("[PluginRestore] creating runtime instance...");
        if is_builtin {
            eprintln!("[PluginRestore] resolved built-in={class_id}");
        } else if is_audio_unit {
            eprintln!("[PluginRestore] resolved audio unit component={class_id}");
        } else {
            eprintln!("[PluginRestore] resolved path={path_string}");
        }

        // An Audio Unit has no module file, so a missing path says nothing about
        // whether the component is installed — the host's load reports that.
        if !is_builtin && !is_audio_unit && path.is_none_or(|path| !path.exists()) {
            let reason = path
                .map(|path| format!("Plugin file not found: {}", path.display()))
                .unwrap_or_else(|| "Plugin file not found".to_string());
            eprintln!("[PluginRestore] failed reason={reason}");
            let _ = self.timeline.update(cx, |timeline, _cx| {
                timeline.state.set_insert_runtime(
                    track_id,
                    slot_id,
                    PluginRuntimeBackend::ExternalBridge,
                    PluginRuntimeState::Missing(reason),
                    None,
                );
            });
            return false;
        }

        let sample_rate = self.current_audio_sample_rate();
        let max_block_size = self
            .audio_bridge
            .engine
            .as_ref()
            .map(|engine| engine.config().buffer_size)
            .unwrap_or(256);
        let descriptor = super::plugin_bridge_runtime::BridgePluginDescriptor {
            track_id: track_id.to_string(),
            insert_id: slot_id.to_string(),
            plugin_path: path_string.clone(),
            class_id: class_id.clone(),
            display_name: display_name.clone(),
            format: slot.plugin_format.map(|f| f.label().to_string()),
        };

        match super::plugin_bridge_runtime::PluginBridgeRuntime::ensure_shared(
            &mut self.plugin_editors.bridge_runtime,
        ) {
            Ok(runtime) => {
                let load_requested = runtime
                    .lock()
                    .ok()
                    .map(|runtime| runtime.has_load_request(slot_id))
                    .unwrap_or(false);
                if load_requested {
                    eprintln!(
                        "[PLUGIN_LOAD_REQUEST_DEDUP]\nplugin_instance_id={slot_id}\nexisting_load_state=requested_or_loaded\nnew_request_created=false\nreason=already_has_load_request"
                    );
                    eprintln!(
                        "[PluginRestore] runtime instance already requested instance_id={slot_id}; reusing bridge"
                    );
                    if !self.plugin_restore_batch_active {
                        self.sync_plugin_bridge_sinks_to_engine(cx, "plugin_restore_reuse");
                    }
                    self.mark_dirty();
                    return true;
                }
                eprintln!(
                    "[PLUGIN_LOAD_REQUEST_DEDUP]\nplugin_instance_id={slot_id}\nexisting_load_state=not_loaded\nnew_request_created=true\nreason=first_load"
                );
                let host_pid = runtime.lock().ok().and_then(|r| r.host_pid());
                let _ = self.timeline.update(cx, |timeline, _cx| {
                    timeline.state.set_insert_runtime(
                        track_id,
                        slot_id,
                        PluginRuntimeBackend::ExternalBridge,
                        PluginRuntimeState::Loading,
                        host_pid,
                    );
                });
                let bridge_sink = match runtime.lock() {
                    Ok(mut runtime) => {
                        let load_result = if is_builtin {
                            // Project restore: hand the persisted state blob to
                            // the host so the DSP is built already configured —
                            // immune to the engine graph/param-ring sync race
                            // that made replayed params silently drop.
                            let state_json = slot
                                .vst3_state
                                .as_deref()
                                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
                            runtime.send_load_builtin_plugin(
                                descriptor,
                                sample_rate,
                                max_block_size,
                                state_json,
                            )
                        } else if is_audio_unit {
                            // Like the built-ins, an AU takes its persisted state
                            // with the load: the host applies it before the
                            // instance reaches the audio producer, so there is no
                            // separate SetPluginState to race the graph sync.
                            runtime.send_load_au_plugin(
                                descriptor,
                                sample_rate,
                                max_block_size,
                                slot.vst3_state.as_deref().map(Vec::as_slice),
                            )
                        } else {
                            runtime.send_load_plugin(descriptor, sample_rate, max_block_size)
                        };
                        if let Err(error) = load_result {
                            eprintln!("[PluginRestore] failed reason={error}");
                            let _ = self.timeline.update(cx, |timeline, _cx| {
                                timeline.state.set_insert_runtime(
                                    track_id,
                                    slot_id,
                                    PluginRuntimeBackend::ExternalBridge,
                                    PluginRuntimeState::Failed(error.to_string()),
                                    host_pid,
                                );
                            });
                            return false;
                        }
                        eprintln!(
                            "[PluginRestore] runtime instance created instance_id={slot_id} source={}",
                            if is_builtin || is_audio_unit {
                                class_id.as_str()
                            } else {
                                path_string.as_str()
                            }
                        );
                        // Restore opaque VST3 state only for external modules;
                        // built-in and AU state already travelled with the load.
                        if !is_builtin && !is_audio_unit {
                            if let Some(state) = slot.vst3_state.as_ref() {
                                if let Err(error) = runtime.send_plugin_state(slot_id, state) {
                                    eprintln!(
                                        "[PluginRestore] SetPluginState send failed instance={slot_id}: {error}"
                                    );
                                }
                            }
                        }
                        runtime.audio_sink_for(slot_id)
                    }
                    Err(_) => {
                        eprintln!("[PluginRestore] failed reason=bridge runtime lock poisoned");
                        return false;
                    }
                };
                let _ = bridge_sink;
                // Publishing the sink and forcing a graph rebuild here is right
                // for a plugin added by hand, and wrong for the dozen a project
                // open restores: it rebuilt the whole engine graph once per
                // plugin, each rebuild resolving every track, insert and buffer
                // in the project. The restore driver does both once, after the
                // batch, when every sink exists.
                self.audio_bridge.project_dirty = true;
                if !self.plugin_restore_batch_active {
                    self.sync_plugin_bridge_sinks_to_engine(cx, "plugin_restore");
                    if let Some(engine) = self.audio_bridge.engine.as_ref() {
                        eprintln!(
                            "[PluginAdd] was_playing={} state_before={:?} source=plugin_restore",
                            engine.transport_playing(),
                            engine.engine_state(),
                        );
                    }
                    self.schedule_audio_project_sync(cx, true, "plugin_restore");
                }
                self.mark_dirty();
                true
            }
            Err(error) => {
                eprintln!("[PluginRestore] failed reason={error}");
                let _ = self.timeline.update(cx, |timeline, _cx| {
                    timeline.state.set_insert_runtime(
                        track_id,
                        slot_id,
                        PluginRuntimeBackend::ExternalBridge,
                        PluginRuntimeState::Failed(error.to_string()),
                        None,
                    );
                });
                false
            }
        }
    }

    /// Tear down external-bridge plugin instances when leaving a project so the
    /// next open always recreates DSP from persisted inserts (not stale host
    /// state left over from the previous session).
    pub(super) fn unload_all_bridge_plugins_for_project_close(&mut self, cx: &mut Context<Self>) {
        if !super::plugin_bridge_runtime::bridge_enabled() {
            return;
        }
        let slots = self.bridge_audio_insert_slots(cx);
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            for (_, insert_id) in &slots {
                let _ = engine.set_plugin_bridge_sink(insert_id.clone(), None);
            }
        }
        if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
            if let Ok(mut runtime) = runtime.lock() {
                for instance_id in runtime.loaded_instance_ids() {
                    runtime.unload_plugin(instance_id);
                }
            }
        }
    }

    /// After a project document is applied to the timeline, drop stale bridge
    /// instances before the awaitable restore batch runs.
    pub(super) fn prepare_bridge_plugin_restore_batch(&mut self, cx: &mut Context<Self>) {
        // A new project document owns different insert ids — the built-in
        // state mirror from the previous document must not leak across.
        // Entries for the incoming project reseed from the loaded blobs.
        crate::components::builtin_plugin_editor::builtin_state_clear();
        // Seed the incoming project's mirror before either the host runtime or
        // an editor window can observe defaults.
        let persisted_builtin_states: Vec<(String, String, std::sync::Arc<Vec<u8>>)> = {
            let state = &self.timeline.read(cx).state;
            state
                .tracks
                .iter()
                .flat_map(|track| track.inserts.iter())
                .chain(state.master.inserts.iter())
                .filter_map(|slot| {
                    let plugin_id = slot.plugin_id.as_deref()?;
                    if !SpherePluginHost::builtin_audio_bridge_supported(plugin_id) {
                        return None;
                    }
                    Some((
                        plugin_id.to_string(),
                        slot.id.clone(),
                        slot.vst3_state.clone()?,
                    ))
                })
                .collect()
        };
        for (plugin_id, insert_id, state) in persisted_builtin_states {
            crate::components::builtin_plugin_editor::builtin_state_seed(
                &plugin_id,
                &insert_id,
                state.as_slice(),
            );
        }
        if !super::plugin_bridge_runtime::bridge_enabled() {
            eprintln!(
                "[PluginRestore] in-process path — engine sync will instantiate native inserts"
            );
            return;
        }

        let inserts = self.bridge_audio_insert_slots(cx);
        let wanted: std::collections::HashSet<String> =
            inserts.iter().map(|(_, slot_id)| slot_id.clone()).collect();
        if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
            if let Ok(mut runtime) = runtime.lock() {
                let stale: Vec<String> = runtime
                    .loaded_instance_ids()
                    .into_iter()
                    .filter(|id| !wanted.contains(id))
                    .collect();
                for id in stale {
                    runtime.unload_plugin(id);
                }
            }
        }
        eprintln!(
            "[PluginRestore] prepared restore batch for {} insert(s)",
            inserts.len()
        );
    }

    /// After a project document is applied to the timeline, recreate runtime
    /// plugin instances for persisted inserts (bridge path).
    pub(super) fn restore_plugin_inserts_after_project_load(&mut self, cx: &mut Context<Self>) {
        if !super::plugin_bridge_runtime::bridge_enabled() {
            eprintln!(
                "[PluginRestore] in-process path — engine sync will instantiate native inserts"
            );
            return;
        }

        let inserts = self.bridge_audio_insert_slots(cx);

        let wanted: std::collections::HashSet<String> =
            inserts.iter().map(|(_, slot_id)| slot_id.clone()).collect();
        if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
            if let Ok(mut runtime) = runtime.lock() {
                let stale: Vec<String> = runtime
                    .loaded_instance_ids()
                    .into_iter()
                    .filter(|id| !wanted.contains(id))
                    .collect();
                for id in stale {
                    runtime.unload_plugin(id);
                }
            }
        }

        eprintln!(
            "[PluginRestore] scheduling restore for {} insert(s)",
            inserts.len()
        );
        for (track_id, slot_id) in inserts {
            // Auto-load ONLY genuinely-unloaded slots. Never auto-reload a slot
            // that is Loading (in progress), Failed/Missing (terminal — manual
            // retry only, spec item 12: no automatic infinite retry), or already
            // loaded / mid-editor-open. This is what stops the Failed->reload
            // and EditorOpening->reload cycles.
            let needs_load = self
                .timeline
                .read(cx)
                .state
                .find_insert_slot(&track_id, &slot_id)
                .map(|slot| {
                    matches!(
                        slot.runtime_state,
                        PluginRuntimeState::NotLoaded | PluginRuntimeState::Unloaded
                    )
                })
                .unwrap_or(false);
            if needs_load {
                let _ = self.load_bridge_insert_for_slot(&track_id, &slot_id, cx);
            }
        }
        self.sync_plugin_bridge_sinks_to_engine(cx, "plugin_restore_batch");
    }

    pub(super) fn open_plugin_manager_external_window(
        &mut self,
        owner_bounds: Option<Bounds<gpui::Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = self.external_windows.plugin_manager.clone() {
            if handle
                .update(cx, |_pm, window, _cx| window.activate_window())
                .is_ok()
            {
                return;
            }
            self.external_windows.plugin_manager = None;
        }

        self.menu_bar.open_menu_id = None;
        self.menu_bar.submenu_path.clear();
        self.overlay.open_popover = None;
        self.overlay.text_context_menu = None;

        let owner_bounds = crate::window_position::resolve_owner_bounds_with_preferred(
            owner_bounds,
            self.studio_window_bounds(cx),
            cx,
        );

        match open_plugin_manager_window(owner_bounds, cx) {
            Ok(handle) => self.external_windows.plugin_manager = Some(handle),
            Err(err) => eprintln!("[plugin-manager] failed to open window: {err}"),
        }
    }
}

/// Where an insert a cache addresses as `(track_id, insert_id)` is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InsertKeyStatus {
    /// Still on `track_id`.
    Live,
    /// On another channel now — moved there, and alive. Never unloaded:
    /// unloading is by instance id alone and would silence it where it plays.
    Moved { track_id: String },
    /// No channel holds it any more.
    Removed,
}

/// How a plug-in is named to the user when its state could not be captured:
/// the insert's name and the channel it is on, or the bare instance id when
/// the insert is gone.
fn plugin_state_label(
    state: &crate::components::timeline::timeline_state::TimelineState,
    instance_id: &str,
) -> String {
    if let Some(slot) = state
        .master
        .inserts
        .iter()
        .find(|slot| slot.id == instance_id)
    {
        return format!("{} on Master", slot.display_name);
    }
    state
        .tracks
        .iter()
        .find_map(|track| {
            let slot = track.inserts.iter().find(|slot| slot.id == instance_id)?;
            Some(format!("{} on {}", slot.display_name, track.name))
        })
        .unwrap_or_else(|| instance_id.to_string())
}

/// What a plug-in state capture is for. Decides whether a plug-in that did
/// not answer in time keeps the session unsaved, and how the warning reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PluginStateCaptureFor {
    /// Save / Save As, and the synchronous saves (a new project, save before
    /// recording): the session is bound to the file written.
    Save,
    /// The Save answer to "Save changes?" on the way to closing or switching
    /// the project: the session is bound to the file, then left.
    SaveThenContinue,
    /// File → Save Copy: the session is not bound to the copy.
    SaveCopy,
    /// An autosave, or the recovery snapshot written on a project switch.
    Autosave,
    /// Audio export: no project file is written.
    Export,
}

impl PluginStateCaptureFor {
    /// Whether a plug-in that missed the capture keeps a dirty session dirty
    /// past the write it was captured for.
    ///
    /// Only after a plain save: the file then lacks what the plug-in holds,
    /// and saving again can capture it. A save on the way to closing or
    /// switching must let that go on — a mark after its snapshot would count
    /// as an edit during the save, and the close or switch would ask
    /// "Save changes?" again for as long as the plug-in does not answer. The
    /// other writers never clear the dirty flag, so there is nothing to keep.
    pub(super) fn keeps_session_dirty(self, session_dirty: bool) -> bool {
        session_dirty && self == Self::Save
    }
}

/// Background task reporting plug-ins whose state a capture missed.
const PLUGIN_STATE_CAPTURE_TASK_ID: &str = "plugin-state-capture";

/// Drop the missed-capture report when the session it was about is replaced:
/// it names that session's plug-ins and tracks, and a failed task would
/// otherwise stay in the next project's status bar. Returns whether one was
/// dropped.
pub(super) fn forget_plugin_state_capture_report(
    tasks: &mut crate::components::BackgroundTaskStore,
) -> bool {
    tasks.tasks.remove(PLUGIN_STATE_CAPTURE_TASK_ID).is_some()
}

/// The warning for plug-ins that did not hand over their state in time,
/// worded for what the capture was for. It is posted when the snapshot is
/// taken, before the write runs, so it says what the snapshot holds and
/// never how the write turned out.
fn uncaptured_plugin_states_message(names: &[String], capture: PluginStateCaptureFor) -> String {
    let (noun, subject, object) = if names.len() == 1 {
        ("plug-in", "Its", "its")
    } else {
        ("plug-ins", "Their", "their")
    };
    let outcome = match capture {
        PluginStateCaptureFor::Save => format!(
            "{subject} last captured state was kept, so the project stays unsaved; \
             save again to capture it."
        ),
        PluginStateCaptureFor::SaveThenContinue => {
            format!("The save uses {object} last captured state.")
        }
        PluginStateCaptureFor::SaveCopy => {
            format!("The copy uses {object} last captured state.")
        }
        PluginStateCaptureFor::Autosave => {
            format!("The autosave uses {object} last captured state.")
        }
        PluginStateCaptureFor::Export => format!("{subject} last captured state was kept."),
    };
    format!(
        "{} {noun} did not answer in time: {}. {outcome}",
        names.len(),
        names.join(", ")
    )
}

/// Classify one `(track_id, insert_id)` key against the project as it is now.
pub(super) fn classify_insert_key(
    state: &crate::components::timeline::timeline_state::TimelineState,
    track_id: &str,
    insert_id: &str,
) -> InsertKeyStatus {
    if state.find_insert_slot(track_id, insert_id).is_some() {
        return InsertKeyStatus::Live;
    }
    match state
        .insert_owner_ids_containing(insert_id)
        .into_iter()
        .next()
    {
        Some(owner) => InsertKeyStatus::Moved { track_id: owner },
        None => InsertKeyStatus::Removed,
    }
}

/// What a channel's editor window does when plug-ins on its tabs have left
/// the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EditorTabDetach {
    /// Tabs the window keeps, in their current order.
    pub remaining: Vec<String>,
    /// Nothing is left on it: close the window.
    pub close_window: bool,
    /// The tab on show left: bring this one to the front.
    pub activate: Option<String>,
    /// The insert the window is filed under left: file it under this one.
    pub rekey: Option<String>,
}

/// Plan a channel window's response to `left` (whether an insert has left
/// this channel). `key_insert` is the insert the window is filed under,
/// `active_insert` the tab on show. `None` when nothing on the window left.
pub(super) fn plan_editor_tab_detach(
    tabs: &[String],
    key_insert: &str,
    active_insert: &str,
    left: impl Fn(&str) -> bool,
) -> Option<EditorTabDetach> {
    if !tabs.iter().any(|id| left(id)) && !left(key_insert) && !left(active_insert) {
        return None;
    }
    let remaining: Vec<String> = tabs.iter().filter(|id| !left(id)).cloned().collect();
    let Some(next) = remaining.first().cloned() else {
        return Some(EditorTabDetach {
            remaining,
            close_window: true,
            activate: None,
            rekey: None,
        });
    };
    Some(EditorTabDetach {
        close_window: false,
        activate: left(active_insert).then(|| next.clone()),
        rekey: left(key_insert).then_some(next),
        remaining,
    })
}

/// Resize shell/content to preferred size before attach (no `ResizeEditor` yet).
fn resize_shell_before_attach(session: &mut BridgeEditorSession, width: u32, height: u32) {
    // Host-owned: the host process owns the editor window and sizes it itself
    // (IPlugView::getSize during embed). The main app has no window to resize
    // and must not echo a size back, or it would shrink the host window.
    if session.shell.is_host_owned_proxy() {
        return;
    }
    let (req_w, req_h) = (width as i32, height as i32);
    if req_w <= 0 || req_h <= 0 {
        eprintln!(
            "[plugin-editor-window] preferred_size_invalid reason=non_positive ({req_w}x{req_h})"
        );
        return;
    }
    eprintln!(
        "[plugin-editor-window] pre_attach_resize requested_content={req_w}x{req_h} instance={}",
        session.instance_id
    );
    let (cw, ch, clamped) = session.shell.clamp_content_to_work_area(req_w, req_h);
    if clamped {
        eprintln!("[plugin-editor-window] preferred_size_clamped=true");
    }
    let recenter = !session.shell.has_user_moved();
    session.shell.resize_to_content(cw, ch, recenter);
    let (final_cw, final_ch) = session.shell.apply_content_layout();
    session.last_content = (final_cw, final_ch);
    session.preferred_applied = true;
    session.shell.ensure_visible_zorder();
    eprintln!(
        "[plugin-editor-window] pre_attach_resize content={final_cw}x{final_ch} instance={}",
        session.instance_id
    );
}

fn log_bridge_gpu_diagnostics(
    session: &BridgeEditorSession,
    plugin_instance_id: &str,
    plugin_path: &str,
) {
    let stats = session.shell.paint_stats();
    crate::components::gpu_editor_diagnostics::log_window_style_audit(
        session.shell.top_hwnd(),
        session.shell.content_hwnd(),
        session.host_hwnd,
    );
    crate::components::gpu_editor_diagnostics::log_gpu_editor_diagnostics(
        plugin_instance_id,
        plugin_path,
        session.shell.top_hwnd(),
        session.shell.content_hwnd(),
        session.host_hwnd,
        stats.content_paint_count,
        stats.content_erase_count,
        stats.size_count,
    );
}

/// Apply the plug-in's preferred size to a native editor shell exactly once:
/// validate, clamp to monitor work area, resize shell + content HWND, optionally
/// recenter if the user has not moved the window, and push `ResizeEditor` (spec
/// Part 3–6). No-op once already applied so later hints don't fight user resize.
fn apply_bridge_preferred(
    session: &mut BridgeEditorSession,
    runtime: Option<&super::plugin_bridge_runtime::SharedPluginBridgeRuntime>,
    width: u32,
    height: u32,
) {
    if session.preferred_applied {
        return;
    }
    session.preferred_applied = true;

    // Host-owned: the host sized its own window to the plug-in's preferred size.
    // The main app owns no window and must not push a (proxy-derived) size back.
    if session.shell.is_host_owned_proxy() {
        return;
    }

    let (req_w, req_h) = (width as i32, height as i32);
    if req_w <= 0 || req_h <= 0 {
        eprintln!("[plugin-editor-window] preferred_size_valid=false");
        eprintln!(
            "[plugin-editor-window] preferred_size_invalid reason=non_positive ({req_w}x{req_h})"
        );
        return;
    }

    eprintln!("[plugin-editor-window] preferred_size_valid=true");
    eprintln!(
        "[plugin-editor-window] auto_size requested_content={req_w}x{req_h} instance={}",
        session.instance_id
    );

    let (cw, ch, clamped) = session.shell.clamp_content_to_work_area(req_w, req_h);
    if clamped {
        eprintln!("[plugin-editor-window] preferred_size_clamped=true");
    }
    eprintln!(
        "[plugin-editor-window] auto_size clamped_content={cw}x{ch} instance={}",
        session.instance_id
    );

    let recenter = !session.shell.has_user_moved();
    session.shell.resize_to_content(cw, ch, recenter);
    let (final_cw, final_ch) = session.shell.content_size();
    let (shell_w, shell_h) = session.shell.shell_outer_size();
    session.last_content = (final_cw, final_ch);
    if let Some(rt) = runtime {
        if let Ok(mut r) = rt.lock() {
            r.resize_editor(
                session.instance_id.clone(),
                final_cw as u32,
                final_ch as u32,
                bridge_editor_dpi(session),
            );
        }
    }
    session.shell.ensure_visible_zorder();
    eprintln!(
        "[plugin-editor-window] resize shell={shell_w}x{shell_h} content={final_cw}x{final_ch} instance={}",
        session.instance_id
    );
    eprintln!(
        "[plugin-bridge] sending ResizeEditor instance={} width={final_cw} height={final_ch}",
        session.instance_id
    );
}

#[cfg(target_os = "windows")]
fn studio_native_hwnd(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get() as u64),
        _ => None,
    }
}

// On Linux the external plugin host owns a top-level GTK4/X11 editor window;
// the returned X11 window id is a read-only owner/DPI reference (never a real
// cross-process parent). Under XWayland GPUI uses the X11 backend so this
// resolves; on pure Wayland there is no X11 owner and VST3 X11 embedding is not
// available, so this returns None (the caller surfaces a clear error).
#[cfg(not(target_os = "windows"))]
fn studio_native_hwnd(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Xcb(w) => Some(w.window.get() as u64),
        RawWindowHandle::Xlib(w) => Some(u64::from(w.window)),
        _ => None,
    }
}

/// Emit paint-instrumentation counters for a native editor shell (spec Part 6).
fn log_bridge_paint_stats(session: &BridgeEditorSession) {
    let stats = session.shell.paint_stats();
    eprintln!(
        "[plugin-editor-paint] instance={} content_paint_count={} content_erase_count={} \
         shell_paint_count={} size_count={}",
        session.instance_id,
        stats.content_paint_count,
        stats.content_erase_count,
        stats.shell_paint_count,
        stats.size_count
    );
}

#[cfg(test)]
mod insert_ownership_tests {
    use super::{EditorTabDetach, InsertKeyStatus, classify_insert_key, plan_editor_tab_detach};
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, InsertPluginFormat, TimelineState, TrackType,
    };

    fn state_with_two_tracks() -> (TimelineState, String, String, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let mut ids = Vec::new();
        for name in ["A", "B"] {
            ids.push(state.create_track(CreateTrackOptions {
                track_type: TrackType::Audio,
                name: name.to_string(),
                color: gpui::Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            }));
        }
        let slot = state.ensure_insert_slot_at(&ids[0], 0).expect("slot");
        state.set_insert_plugin(
            &ids[0],
            &slot,
            "fx".to_string(),
            Some(std::path::PathBuf::from("C:/p/fx.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "FX".to_string(),
        );
        (state, ids[0].clone(), ids[1].clone(), slot)
    }

    /// A key whose insert moved is "moved", never "removed" — the reconcile
    /// sweep unloads only removed instances, so a moved one keeps playing.
    #[test]
    fn a_moved_insert_is_never_classified_for_unload() {
        let (mut state, a, b, slot) = state_with_two_tracks();
        assert_eq!(
            classify_insert_key(&state, &a, &slot),
            InsertKeyStatus::Live
        );

        let plan = state.plan_insert_move(&a, &slot, &b, 0).expect("movable");
        assert!(state.apply_insert_move(&plan));
        assert_eq!(
            classify_insert_key(&state, &a, &slot),
            InsertKeyStatus::Moved {
                track_id: b.clone()
            }
        );
        assert_eq!(
            classify_insert_key(&state, &b, &slot),
            InsertKeyStatus::Live
        );

        state.remove_insert(&b, &slot);
        assert_eq!(
            classify_insert_key(&state, &a, &slot),
            InsertKeyStatus::Removed
        );
    }

    fn tabs(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// Moving the plug-in a tabbed window is filed under leaves the channel's
    /// other tabs where they are: the window re-files under one of them and
    /// stays on the tab it was showing.
    #[test]
    fn moving_the_key_insert_keeps_the_other_tabs_on_the_source_channel() {
        let plan = plan_editor_tab_detach(&tabs(&["x", "y", "z"]), "x", "y", |id| id == "x");
        assert_eq!(
            plan,
            Some(EditorTabDetach {
                remaining: tabs(&["y", "z"]),
                close_window: false,
                activate: None,
                rekey: Some("y".into()),
            })
        );
    }

    #[test]
    fn moving_the_tab_on_show_brings_another_to_the_front() {
        let plan = plan_editor_tab_detach(&tabs(&["x", "y"]), "x", "y", |id| id == "y");
        assert_eq!(
            plan,
            Some(EditorTabDetach {
                remaining: tabs(&["x"]),
                close_window: false,
                activate: Some("x".into()),
                rekey: None,
            })
        );
    }

    #[test]
    fn moving_the_only_tab_closes_the_window_and_nothing_moved_does_nothing() {
        let plan = plan_editor_tab_detach(&tabs(&["x"]), "x", "x", |id| id == "x");
        assert!(plan.is_some_and(|plan| plan.close_window && plan.remaining.is_empty()));
        assert_eq!(
            plan_editor_tab_detach(&tabs(&["x", "y"]), "x", "x", |_| false),
            None
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeEditorState, EditorReopenAction, PluginEditorWindows, bridge_editor_is_open,
        bridge_editor_is_terminal, editor_reopen_action,
    };

    // Every non-terminal, non-Loading state. These are "live or in flight" and
    // a re-open must focus the existing shell rather than spawn a duplicate.
    const FOCUS_STATES: &[BridgeEditorState] = &[
        BridgeEditorState::ParentWindowCreated,
        BridgeEditorState::ViewCreated,
        BridgeEditorState::Sized,
        BridgeEditorState::AwaitingAttach,
        BridgeEditorState::Attached,
        BridgeEditorState::Visible,
        BridgeEditorState::Ready,
    ];

    #[test]
    fn removed_instance_discards_only_its_deferred_editor_open() {
        let mut windows = PluginEditorWindows::default();
        windows.deferred_opens = vec![
            ("track-a".into(), 0, "instance-a".into()),
            ("track-b".into(), 1, "instance-b".into()),
        ];
        windows.flush_attempts.insert("instance-a".into(), 3);
        windows.flush_attempts.insert("instance-b".into(), 1);

        windows.discard_pending_open("instance-a");

        assert_eq!(
            windows.deferred_opens,
            vec![("track-b".into(), 1, "instance-b".into())]
        );
        assert!(!windows.flush_attempts.contains_key("instance-a"));
        assert_eq!(windows.flush_attempts.get("instance-b"), Some(&1));
    }

    #[test]
    fn terminal_states_drop_and_retry() {
        // The core spec contract: a Failed / timed-out session is NEVER treated
        // as live — re-open drops it and opens fresh (the "cannot open the
        // editor again" regression guard).
        for state in [
            BridgeEditorState::Failed("boom".to_string()),
            BridgeEditorState::TimedOut("slow".to_string()),
        ] {
            assert!(bridge_editor_is_terminal(&state));
            assert!(!bridge_editor_is_open(&state));
            assert_eq!(
                editor_reopen_action(&state),
                EditorReopenAction::DropAndRetry
            );
        }
    }

    #[test]
    fn loading_state_reuses_shell() {
        let state = BridgeEditorState::Loading;
        assert!(!bridge_editor_is_open(&state));
        assert!(!bridge_editor_is_terminal(&state));
        assert_eq!(
            editor_reopen_action(&state),
            EditorReopenAction::ReuseLoadingShell
        );
    }

    #[test]
    fn live_and_inflight_states_focus_existing() {
        for state in FOCUS_STATES {
            assert!(!bridge_editor_is_terminal(state));
            assert_eq!(
                editor_reopen_action(state),
                EditorReopenAction::FocusExisting
            );
        }
    }

    #[test]
    fn only_attached_visible_ready_count_as_open() {
        assert!(bridge_editor_is_open(&BridgeEditorState::Attached));
        assert!(bridge_editor_is_open(&BridgeEditorState::Visible));
        assert!(bridge_editor_is_open(&BridgeEditorState::Ready));
        // Intermediate in-flight states are focusable but not "open" yet.
        assert!(!bridge_editor_is_open(&BridgeEditorState::AwaitingAttach));
    }
}

#[cfg(test)]
mod plugin_state_dirty_tests {
    use std::time::{Duration, Instant};

    use super::{
        PLUGIN_EDIT_GESTURE_GAP, PLUGIN_STATE_CAPTURE_TASK_ID, PluginEditGesture, PluginEditSettle,
        PluginEditStep, PluginStateCaptureFor, forget_plugin_state_capture_report,
        plugin_state_label, uncaptured_plugin_states_message,
    };
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, InsertPluginFormat, TimelineState, TrackType,
    };
    use crate::project::ProjectSession;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    const OPENS: PluginEditStep = PluginEditStep {
        mark: true,
        arm_settle: true,
    };
    const CONTINUES: PluginEditStep = PluginEditStep {
        mark: false,
        arm_settle: false,
    };

    /// A knob drag marks the project once, not once per frame.
    #[test]
    fn a_gesture_marks_the_project_once_when_it_starts() {
        let t0 = Instant::now();
        let mut gesture = PluginEditGesture::default();
        assert_eq!(gesture.edit(t0, 1), OPENS);
        for frame in 1..=30 {
            assert_eq!(gesture.edit(t0 + ms(16 * frame), 1), CONTINUES);
        }
    }

    /// The trailing mark comes only once the edits stop, and only when edits
    /// followed the opening mark: a single click needs no second one.
    #[test]
    fn the_end_of_a_gesture_marks_again_only_after_later_edits() {
        let t0 = Instant::now();
        let mut click = PluginEditGesture::default();
        click.edit(t0, 1);
        assert_eq!(
            click.settle(t0 + PLUGIN_EDIT_GESTURE_GAP + ms(1)),
            PluginEditSettle::Done { mark_session: None }
        );

        let mut drag = PluginEditGesture::default();
        drag.edit(t0, 7);
        drag.edit(t0 + ms(300), 7);
        match drag.settle(t0 + ms(500)) {
            PluginEditSettle::Wait(wait) => {
                assert!(wait > Duration::ZERO && wait <= PLUGIN_EDIT_GESTURE_GAP)
            }
            other => panic!("the drag is still going, got {other:?}"),
        }
        assert_eq!(
            drag.settle(t0 + ms(300) + PLUGIN_EDIT_GESTURE_GAP + ms(1)),
            PluginEditSettle::Done {
                mark_session: Some(7)
            }
        );
        // Settled: the next edit opens a new gesture and arms a new check.
        assert_eq!(drag.edit(t0 + ms(5_000), 7), OPENS);
    }

    #[test]
    fn a_pause_or_another_session_starts_a_new_gesture() {
        let t0 = Instant::now();
        let mut gesture = PluginEditGesture::default();
        gesture.edit(t0, 1);
        // Paused past the gap while its settle check is still armed: marks,
        // and the armed check serves the new gesture.
        let step = gesture.edit(t0 + PLUGIN_EDIT_GESTURE_GAP + ms(1), 1);
        assert_eq!(
            step,
            PluginEditStep {
                mark: true,
                arm_settle: false
            }
        );
        // The project was switched: its first edit is marked on its own.
        assert!(gesture.edit(t0 + PLUGIN_EDIT_GESTURE_GAP + ms(2), 2).mark);
    }

    /// Why the gesture has a trailing mark: a save that snapshots mid-drag
    /// must not report the rest of the drag as saved.
    #[test]
    fn edits_after_a_save_snapshot_keep_the_session_dirty() {
        let t0 = Instant::now();
        let mut session = ProjectSession::untitled();
        let mut gesture = PluginEditGesture::default();
        if gesture.edit(t0, 1).mark {
            session.mark_dirty();
        }
        let saved_generation = session.dirty_generation;
        assert!(
            !gesture.edit(t0 + ms(100), 1).mark,
            "mid-gesture, after the snapshot"
        );
        if let PluginEditSettle::Done {
            mark_session: Some(_),
        } = gesture.settle(t0 + ms(100) + PLUGIN_EDIT_GESTURE_GAP + ms(1))
        {
            session.mark_dirty();
        }
        let clean = session.bind_saved_snapshot(
            session.id.clone(),
            "Song".to_string(),
            None,
            std::path::PathBuf::from("/tmp/Song/Song.fbproj"),
            1,
            2,
            saved_generation,
        );
        assert!(!clean);
        assert!(session.is_dirty);
    }

    #[test]
    fn plug_ins_that_did_not_answer_are_named_by_insert_and_channel() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Lead Vox".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track, 0).expect("slot");
        state.set_insert_plugin(
            &track,
            &slot,
            "fx".to_string(),
            Some(std::path::PathBuf::from("/p/Comp.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Comp".to_string(),
        );
        assert_eq!(plugin_state_label(&state, &slot), "Comp on Lead Vox");
        assert_eq!(plugin_state_label(&state, "gone-7"), "gone-7");

        let save = PluginStateCaptureFor::Save;
        let one = uncaptured_plugin_states_message(&["Comp on Lead Vox".to_string()], save);
        assert!(one.starts_with("1 plug-in did not answer in time: Comp on Lead Vox."));
        assert!(one.contains("Its last captured state was kept"));
        let two =
            uncaptured_plugin_states_message(&["A on 1".to_string(), "B on 2".to_string()], save);
        assert!(two.starts_with("2 plug-ins did not answer in time: A on 1, B on 2."));
        assert!(two.contains("Their last captured state was kept"));
    }

    /// Only a plain save keeps the project unsaved when a plug-in missed the
    /// capture. A save on the way to closing or switching lets that go on:
    /// re-marking after its snapshot would ask "Save changes?" again for as
    /// long as the plug-in does not answer.
    #[test]
    fn only_a_plain_save_keeps_the_session_dirty_after_a_missed_capture() {
        use PluginStateCaptureFor::*;
        assert!(Save.keeps_session_dirty(true));
        for capture in [SaveThenContinue, SaveCopy, Autosave, Export] {
            assert!(!capture.keeps_session_dirty(true), "{capture:?}");
        }
        for capture in [Save, SaveThenContinue, SaveCopy, Autosave, Export] {
            assert!(
                !capture.keeps_session_dirty(false),
                "a clean session stays clean: {capture:?}"
            );
        }
    }

    /// The warning says what happened for what the capture was for: only a
    /// plain save says the project stays unsaved, and an export claims no
    /// save at all.
    #[test]
    fn the_missed_capture_warning_matches_what_the_capture_was_for() {
        use PluginStateCaptureFor::*;
        let names = ["Comp on Lead Vox".to_string()];
        let message = |capture| uncaptured_plugin_states_message(&names, capture);
        for capture in [Save, SaveThenContinue, SaveCopy, Autosave, Export] {
            assert!(
                message(capture).starts_with("1 plug-in did not answer in time: Comp on Lead Vox."),
                "{capture:?}"
            );
        }
        assert!(message(Save).contains("the project stays unsaved; save again"));
        for capture in [SaveThenContinue, SaveCopy, Autosave, Export] {
            assert!(!message(capture).contains("unsaved"), "{capture:?}");
        }
        // Posted before the write runs: what the snapshot holds, never a
        // claim that the write succeeded.
        assert!(message(SaveThenContinue).ends_with("The save uses its last captured state."));
        assert!(message(SaveCopy).ends_with("The copy uses its last captured state."));
        assert!(message(Autosave).ends_with("The autosave uses its last captured state."));
        for capture in [Save, SaveThenContinue, SaveCopy, Autosave, Export] {
            let message = message(capture);
            for claim in ["is saved", "was saved", "keeps"] {
                assert!(!message.contains(claim), "{capture:?}: {message}");
            }
        }
        let export = message(Export);
        assert!(!export.contains("save"), "{export}");
        assert!(export.ends_with("Its last captured state was kept."));
    }

    /// A save-then-switch goes on after a missed capture; the report names
    /// the old project's plug-ins and must not stay in the next project's
    /// status bar. Other tasks are left alone.
    #[test]
    fn installing_another_project_drops_the_missed_capture_report() {
        use crate::components::{
            BackgroundTaskKind, BackgroundTaskStatus, BackgroundTaskStore, BackgroundTaskUpdate,
        };
        let task = |title: &str, status| BackgroundTaskUpdate {
            kind: BackgroundTaskKind::ProjectSave,
            title: title.to_string(),
            detail: None,
            status,
            progress: None,
            error: None,
            cancellable: false,
            parent_id: None,
        };
        let mut tasks = BackgroundTaskStore::default();
        tasks.add_or_update(
            PLUGIN_STATE_CAPTURE_TASK_ID,
            task("Plug-in state not captured", BackgroundTaskStatus::Running),
        );
        tasks.fail(
            PLUGIN_STATE_CAPTURE_TASK_ID,
            "1 plug-in did not answer in time",
        );
        tasks.add_or_update(
            "project-autosave",
            task("Autosave project", BackgroundTaskStatus::Running),
        );

        assert!(forget_plugin_state_capture_report(&mut tasks));
        assert!(!tasks.tasks.contains_key(PLUGIN_STATE_CAPTURE_TASK_ID));
        assert!(tasks.tasks.contains_key("project-autosave"));
        assert!(!forget_plugin_state_capture_report(&mut tasks));
    }
}
