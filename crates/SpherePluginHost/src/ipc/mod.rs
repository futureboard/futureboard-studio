//! Cross-process IPC protocol between `FutureboardNative.exe` (the GPUI main
//! app, IPC *client*) and `FutureboardPluginHostX64.exe` (the plugin host
//! process, IPC *server*).
//!
//! Transport is **newline-delimited JSON** over the child process's
//! stdin/stdout — one JSON object per line. This mirrors the existing
//! `futureboard_plugin_scanner` precedent (`scan::isolation`), needs no extra
//! dependency, and is trivially loggable/diffable. Commands flow
//! client → host on the host's **stdin**; events flow host → client on the
//! host's **stdout**. The host keeps **stderr** free for human-readable debug
//! logs (gated behind `FUTUREBOARD_PLUGIN_VIEW_DEBUG`).
//!
//! Slice 1 scope: the host owns the VST3 *editor* lifecycle for an HWND created
//! and owned by the main app (`mode = main_owned_window`). The plugin instance
//! is loaded by `plugin_path` + `class_id` (the self-contained path-based
//! loader in `native_editor`); sharing one instance with the audio engine is a
//! later slice. See the plan / `native_editor` module docs.

use std::io::{self, BufRead, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Wire-format version. Bump on any breaking change to [`HostCommand`] /
/// [`HostEvent`]. The client sends it in [`HostCommand::Hello`] and the host
/// echoes its own in [`HostEvent::Ready`]; a mismatch should be surfaced, not
/// silently tolerated.
pub const PROTOCOL_VERSION: u32 = 6;

/// Commands sent **client → host** (written to the host's stdin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum HostCommand {
    /// Handshake; carries the client's protocol version and main-window HWND.
    Hello {
        protocol_version: u32,
        #[serde(default)]
        main_hwnd: Option<u64>,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Liveness handshake — the host replies with [`HostEvent::Pong`]. Sent by
    /// the bridge client right after spawn to confirm the process is alive and
    /// speaking the protocol before any editor command.
    Ping,
    /// Load a plugin instance into the external host runtime. The main app
    /// sends this as soon as a VST3 insert is created; editor attachment is a
    /// later command against the same `plugin_instance_id`.
    LoadPlugin {
        plugin_instance_id: String,
        plugin_path: String,
        class_id: String,
        sample_rate: u32,
        max_block_size: u32,
        /// Module format label (`"VST3"` / `"VST2"`) selecting which native
        /// bridge instantiates the plug-in. Optional and defaulted so an older
        /// host binary still parses the frame; the host then falls back to
        /// detecting the format from `plugin_path`.
        #[serde(default)]
        format: Option<String>,
    },
    /// Create a Futureboard built-in DSP instance in the host process. Built-ins
    /// use the same per-instance shared-memory audio exchange as VST3 plugins,
    /// but do not have a module path or native editor lifecycle.
    LoadBuiltinPlugin {
        plugin_instance_id: String,
        plugin_id: String,
        sample_rate: u32,
        max_block_size: u32,
        /// Persisted state blob (e.g. a `RodhareistState` JSON) applied to the
        /// freshly built DSP *before* it is published to the audio producer —
        /// no engine-graph or param-ring timing involved, so project restore
        /// cannot race the graph sync. `None` = start at defaults.
        #[serde(default)]
        state_json: Option<String>,
    },
    /// Instantiate a macOS Audio Unit in the host process. AU has no module
    /// path: `component_id` is the scanner's `au:<type>:<subtype>:<manufacturer>`
    /// identifier, which resolves to an AudioComponentDescription. Like the
    /// built-ins, AU is hosted only here — there is no in-process engine path —
    /// so state travels with the load rather than through a later command, and is
    /// applied before the instance is published to the audio producer.
    LoadAudioUnit {
        plugin_instance_id: String,
        component_id: String,
        sample_rate: u32,
        max_block_size: u32,
        /// Opaque state bytes (base64 of the unit's ClassInfo binary plist), as
        /// produced by [`HostEvent::PluginState`]. `None` = factory defaults.
        #[serde(default)]
        state_b64: Option<String>,
    },
    /// Attach a VST3 editor view into an HWND owned by the main app.
    ///
    /// `parent_hwnd` is the main-app-created **content child HWND**
    /// (`content_hwnd != top_hwnd`). HWNDs are process-global on Windows and
    /// travel as a `u64`. The main app must keep the HWND alive until the host
    /// reports [`HostEvent::EditorClosed`].
    OpenEditorWithParentHwnd {
        #[serde(default)]
        track_id: Option<String>,
        #[serde(default)]
        track_index: Option<u32>,
        #[serde(default)]
        track_name: Option<String>,
        #[serde(default)]
        plugin_slot_id: Option<String>,
        plugin_instance_id: String,
        plugin_path: String,
        class_id: String,
        #[serde(default)]
        plugin_uid: Option<String>,
        #[serde(default)]
        plugin_display_name: Option<String>,
        #[serde(default)]
        owner_hwnd: Option<u64>,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    },
    /// Phase 1: `createView` + `getSize` only; host emits [`HostEvent::EditorPreferredSize`].
    PrepareEditorView {
        plugin_instance_id: String,
        plugin_path: String,
        class_id: String,
    },
    /// Phase 2: attach after main app resized content HWND to preferred size.
    ConfirmEditorContentReady {
        plugin_instance_id: String,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    },
    /// Main app resized the content HWND; host re-issues `IPlugView::onSize`.
    ResizeEditor {
        plugin_instance_id: String,
        width: u32,
        height: u32,
        dpi: u32,
    },
    /// Detach the editor view (`IPlugView::removed`) but keep the plugin loaded.
    CloseEditor {
        plugin_instance_id: String,
    },
    /// Detach (if attached) and release the plugin instance entirely.
    UnloadPlugin {
        plugin_instance_id: String,
    },
    /// Preview a single MIDI note on a loaded VSTi instance (transport may be stopped).
    PreviewNoteOn {
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    PreviewNoteOff {
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
    },
    PreviewControlChange {
        plugin_instance_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    },
    PreviewAllNotesOff {
        plugin_instance_id: String,
    },
    MidiPanic {
        plugin_instance_id: String,
    },
    /// Stage 1 (shared audio bridge): the **main engine owns** the sample rate
    /// and block size; the host must *follow* these for all plugin DSP. Sent
    /// before the first `LoadPlugin` and whenever the engine's audio config
    /// changes. Diagnostics-only at this stage — the host applies the config and
    /// replies [`HostEvent::AudioBridgeConfigured`]; no shared-memory audio
    /// transport exists yet (that is Stage 2).
    ConfigureAudioBridge {
        sample_rate: u32,
        max_block_size: u32,
    },
    /// Prepare plugin DSP at the engine-owned sample rate / block size.
    PrepareProcessing {
        plugin_instance_id: String,
        sample_rate: u32,
        max_block_size: u32,
        input_channels: u32,
        output_channels: u32,
    },
    /// Stage 1 skeleton: request the host to process one DSP block of `frames`
    /// samples. The lock-free shared-memory audio/MIDI transport is Stage 2/3;
    /// for now the host acknowledges with [`HostEvent::AudioBridgeStatus`]
    /// reporting `dsp_output=pending` (plugin output is NOT yet mixed into the
    /// main engine — never faked through a second device stream).
    ProcessBlockShared {
        block_id: u64,
        frames: u32,
    },
    /// Stage 2: the engine created a named shared-memory region
    /// ([`crate::audio_bridge::SharedAudioBridge`]) and asks the host to map it.
    /// `bytes` is the region size for validation. The host replies
    /// [`HostEvent::SharedAudioAttached`]. The lock-free buffers carry audio
    /// in/out, the MIDI ring, the parameter-automation ring, and the
    /// status/latency/meter block — no heap alloc or blocking on the audio thread.
    AttachSharedAudio {
        name: String,
        bytes: u64,
        /// Insert slot / plugin runtime id this region serves (one region per
        /// instance so multi-insert FX chains get independent request/done_seq).
        #[serde(default)]
        plugin_instance_id: String,
    },
    /// Capture the plugin's current state (VST3 `IComponent::getState` +
    /// `IEditController::getState`) for project persistence. The host replies
    /// [`HostEvent::PluginState`] with base64 blobs.
    GetPluginState {
        plugin_instance_id: String,
    },
    /// Restore a previously captured plugin state. Sent after
    /// `LoadPlugin`/`PrepareProcessing` on project open. Blobs are base64 of
    /// the raw VST3 streams; either may be empty. The host replies
    /// [`HostEvent::PluginStateSet`].
    SetPluginState {
        plugin_instance_id: String,
        component_b64: String,
        controller_b64: String,
    },
    /// Request the automatable VST3 parameter list for a loaded instance.
    /// The host replies [`HostEvent::PluginParameters`].
    GetPluginParameters {
        plugin_instance_id: String,
    },
    /// Load a `.nam` neural capture into a built-in DSP instance (rodharerist's
    /// Tone/Amp slot). `json` is the raw `.nam` file text — the newline-framed
    /// transport caps a line at 128 MiB, ample for real captures. The host
    /// parses/builds on its IPC thread and hands the runtime to the audio
    /// producer for block-boundary adoption; it replies
    /// [`HostEvent::BuiltinNamCaptureResult`] either way.
    LoadBuiltinNamCapture {
        plugin_instance_id: String,
        /// Display name (usually the file stem) echoed back in the result.
        name: String,
        json: String,
        /// Build two independent models (true stereo) vs mirror one.
        stereo: bool,
        /// Capture already models amp + cab + mic ("Bypass Cab" hint).
        full_rig: bool,
    },
    /// Load a `.wav` impulse response into a built-in DSP instance
    /// (rodharerist's Cabinet slot). `wav_b64` is the raw file, base64-encoded
    /// because the transport is newline-framed JSON — an IR is tens of KB, far
    /// inside the 128 MiB line cap. The host decodes, resamples and FFTs on its
    /// IPC thread and hands the runtime to the audio producer for
    /// block-boundary adoption; it replies [`HostEvent::BuiltinIrResult`]
    /// either way.
    LoadBuiltinIr {
        plugin_instance_id: String,
        /// Display name (usually the file name) echoed back in the result.
        name: String,
        wav_b64: String,
    },
    /// What the plug-in editor's chrome strip should show.
    ///
    /// Only the host-owned-window platforms use this. On Windows the plug-in's
    /// view is embedded in the studio's own window and GPUI draws the chrome
    /// above it; on macOS the window belongs to this process, so the strip has
    /// to be drawn here — and everything in it still belongs to the studio,
    /// which is why it arrives already resolved rather than as state the host
    /// would have to interpret.
    ///
    /// Every field is a finished string or a resolved colour. The host formats
    /// nothing: a second implementation of "how a latency reads" or "which
    /// grey a hovered tab is" is a second answer waiting to disagree with the
    /// first.
    SetEditorChrome {
        plugin_instance_id: String,
        /// The window's title bar text.
        title: String,
        /// Whether the insert is processing (bypassed and disabled both read
        /// as `false` here — from the editor's side they say the same thing).
        active: bool,
        /// Already formatted: `12%` / `—`.
        cpu_label: String,
        /// Already formatted: `3.2 ms` / `0 smp`.
        latency_label: String,
        /// Already formatted: the loaded preset, `Unsaved`, or `No presets`.
        preset_label: String,
        /// The preset menu's rows, in menu order.
        presets: Vec<String>,
        /// Which row is loaded, when the studio knows.
        #[serde(default)]
        preset_index: Option<u32>,
        /// Every plug-in open on this channel, in slot order.
        tabs: Vec<EditorChromeTab>,
        /// `tabs` entry this window is showing.
        active_tab: String,
        /// Resolved theme colours, so the strip follows the studio's theme
        /// rather than carrying a palette of its own that only matches by
        /// accident.
        palette: EditorChromePalette,
    },
    /// Graceful host shutdown: detach everything and exit 0.
    Shutdown,
}

/// One plug-in open on the channel this editor window belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorChromeTab {
    /// Insert slot id — how the studio and the host both address it.
    pub insert_id: String,
    /// Plug-in name, as the tab shows it.
    pub display_name: String,
    /// 1-based slot position on the channel.
    pub insert_number: u32,
}

/// The chrome strip's colours, resolved by the studio from the active theme.
///
/// Packed `0xRRGGBBAA`. Resolved rather than named because several of these are
/// composites — a hovered control is its rest fill lifted by a state layer, and
/// working that out needs `Colors::composite` and the theme it reads. Sending
/// the finished colour keeps one place that knows how a state layer lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EditorChromePalette {
    /// Tab strip ground, and an unselected tab.
    pub strip_bg: u32,
    /// Chrome row ground, and the selected tab.
    pub row_bg: u32,
    /// Hairline under each band.
    pub border: u32,
    /// Button and preset-trigger rest fill.
    pub control_bg: u32,
    /// The same, hovered.
    pub control_hover: u32,
    /// The same, pressed.
    pub control_pressed: u32,
    /// A latched control's fill (the power button when the insert is on).
    pub accent: u32,
    pub text_primary: u32,
    pub text_secondary: u32,
    pub text_faint: u32,
}

/// A chrome control the user operated, on its way back to the studio.
///
/// Deliberately the same set as the GPUI chrome's own action enum: the studio
/// applies both through one function, so a control cannot mean one thing on
/// Windows and another on macOS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum EditorChromeCommand {
    /// Turn the insert's processing on or off.
    SetActive { active: bool },
    /// Move `delta` places through the preset list, wrapping.
    StepPreset { delta: i32 },
    /// Store the plug-in's current state as a new preset.
    SavePreset,
    /// Load the preset at this index.
    SelectPreset { index: u32 },
    /// Bring another of this channel's open plug-ins to the front.
    SelectTab { insert_id: String },
    /// Close one plug-in's tab.
    CloseTab { insert_id: String },
}

/// Events sent **host → client** (written to the host's stdout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum HostEvent {
    /// Emitted once at startup. Pairs with [`HostCommand::Hello`].
    Ready { protocol_version: u32, pid: u32 },
    /// Reply to [`HostCommand::Ping`] — confirms the bridge is live.
    Pong { pid: u32 },
    /// The transport key (Space) was pressed while a plug-in editor window
    /// owned by this process had keyboard focus, and no text field in that
    /// window wanted it.
    ///
    /// Keyboard input goes to the thread owning the focused window, so once a
    /// plug-in editor takes focus the main app's window never sees the key —
    /// including for editors embedded into a main-app child HWND, because the
    /// plug-in's view still belongs to this process's UI thread. The host
    /// swallows the key and reports it here; the main app runs its normal
    /// `transport:play-pause` command, so Space means the same thing whether or
    /// not an editor has focus.
    ///
    /// Transport *state* travels the other way through the shared audio bridge
    /// (per-block `ProcessContext`), not over this channel.
    TransportToggleRequested {
        /// Where the key was seen, for diagnostics ("editor" / "window").
        #[serde(default)]
        source: String,
        /// The press's Win32 message time (`MSG.time` / `KBDLLHOOKSTRUCT.time`),
        /// when the claim site had one.
        ///
        /// It identifies the *physical* press, and it is the same tick count in
        /// every process that sees it. The main app can have claimed the same
        /// key itself — an editor embedded in one of its windows is watched by
        /// its own message hook as well as by this process's filters — and
        /// without an identity the two reports become two play/pause commands,
        /// which cancel out and read as the key doing nothing.
        #[serde(default)]
        time_ms: Option<u32>,
    },
    /// Host accepted a load request and is resolving the plugin.
    PluginLoading { plugin_instance_id: String },
    /// Plugin runtime is available in the host process.
    PluginLoaded {
        plugin_instance_id: String,
        name: String,
    },
    /// [`HostCommand::LoadPlugin`] for an instance that is already loaded —
    /// the host reuses the existing component/controller (no second create).
    PluginAlreadyLoaded {
        plugin_instance_id: String,
        name: String,
    },
    /// Plugin load failed; the main app should surface this and must not
    /// silently fall back to in-process hosting while the bridge is enabled.
    PluginLoadFailed {
        plugin_instance_id: String,
        error: String,
    },
    /// Editor view attached to the supplied HWND. `result` is the raw VST3
    /// `tresult` from `attached` (0 == `kResultOk`).
    EditorAttached {
        plugin_instance_id: String,
        result: i32,
        preferred_width: u32,
        preferred_height: u32,
        /// `IPlugView::canResize` at attach time. When false the main app must
        /// lock the wrapper to the preferred content size (+ titlebar) instead
        /// of letting the user drag-resize into blank/garbage area. Defaults
        /// to true so a missing field never locks a resizable editor.
        #[serde(default = "default_editor_resizable")]
        resizable: bool,
        /// Plugin-host child HWND (`IPlugView::attached` target); 0 if unknown.
        #[serde(default)]
        host_hwnd: u64,
    },
    /// Attach failed (bad HWND, plugin load failure, no view, …).
    EditorAttachFailed {
        plugin_instance_id: String,
        error: String,
    },
    /// Plugin-requested preferred content size (host → client hint; the main
    /// app decides the final shell size).
    EditorPreferredSize {
        plugin_instance_id: String,
        width: u32,
        height: u32,
    },
    /// Plug-in called `IPlugFrame::resizeView` — main app should resize shell.
    EditorContentResize {
        plugin_instance_id: String,
        width: u32,
        height: u32,
        #[serde(default)]
        dpi: u32,
    },
    /// Editor view detached (`IPlugView::removed` called).
    EditorClosed { plugin_instance_id: String },
    /// Freeze watchdog: the host UI thread's message pump stalled for
    /// `gap_ms` while this editor was open. The main app may surface a
    /// "plugin editor not responding" hint; the editor close path stays
    /// available because the wrapper window lives in the main process.
    EditorUnresponsive {
        plugin_instance_id: String,
        gap_ms: u64,
    },
    /// Plugin instance released.
    PluginUnloaded { plugin_instance_id: String },
    /// Out-of-band log line (host-side diagnostics surfaced to the client).
    Log { level: String, message: String },
    /// Stage 1 reply to [`HostCommand::ConfigureAudioBridge`]: the host accepted
    /// the engine-owned sample rate / block size and is following them.
    AudioBridgeConfigured {
        sample_rate: u32,
        max_block_size: u32,
        /// True once the host's plugin DSP runs at the engine's rate/block.
        follows_engine: bool,
    },
    /// Stage 1 status for the shared audio bridge. `dsp_output` is `"pending"`
    /// until plugin DSP output is actually mixed into the main engine
    /// (Stage 3) — it is never `"ready"` while audio only plays through a
    /// separate device stream. `latency_samples` is the reported plugin latency
    /// (0 until Stage 4).
    AudioBridgeStatus {
        block_id: u64,
        dsp_output: String,
        latency_samples: u32,
    },
    /// Stage 2 reply to [`HostCommand::AttachSharedAudio`]: whether the host
    /// mapped the shared-memory region and validated its header.
    SharedAudioAttached {
        attached: bool,
        name: String,
        bytes: u64,
    },
    /// Reply to [`HostCommand::PrepareProcessing`]: plugin DSP is active at the
    /// engine-owned rate/block.
    ProcessingPrepared {
        plugin_instance_id: String,
        sample_rate: u32,
        max_block_size: u32,
        output_channels: u32,
        /// Real per-bus output channel counts (bus-by-bus order). Lets the host
        /// model one mixer strip per plugin output bus with correct mono→stereo
        /// duplication instead of pairing flat channels into stereo strips.
        /// `#[serde(default)]` keeps frames from older hosts decodable.
        #[serde(default)]
        output_bus_channels: Vec<u32>,
    },
    /// Reply to [`HostCommand::GetPluginState`]. `ok` is false when the
    /// instance is not loaded or state capture failed; blobs are base64 of the
    /// raw VST3 component/controller streams (either may be empty — a plugin
    /// with no state is valid).
    PluginState {
        plugin_instance_id: String,
        ok: bool,
        component_b64: String,
        controller_b64: String,
    },
    /// Reply to [`HostCommand::SetPluginState`].
    PluginStateSet {
        plugin_instance_id: String,
        ok: bool,
    },
    /// Reply to [`HostCommand::GetPluginParameters`].
    PluginParameters {
        plugin_instance_id: String,
        ok: bool,
        parameters: Vec<HostPluginParameter>,
    },
    /// Reply to [`HostCommand::LoadBuiltinNamCapture`]. On success the capture
    /// has been submitted and will be adopted at the next audio block; on
    /// failure `error` carries the human-readable reason (parse failure,
    /// sample-rate mismatch, unknown instance).
    BuiltinNamCaptureResult {
        plugin_instance_id: String,
        ok: bool,
        /// Display name echoed from the request.
        name: String,
        #[serde(default)]
        error: Option<String>,
        /// Receptive field of the loaded model in samples (0 on failure) —
        /// the capture's contribution to plugin latency.
        #[serde(default)]
        receptive_field: u64,
        /// Capture already models amp + cab + mic ("Bypass Cab" hint).
        #[serde(default)]
        full_rig: bool,
    },
    /// Reply to [`HostCommand::LoadBuiltinIr`]. On success the IR has been
    /// submitted and will be adopted at the next audio block; on failure
    /// `error` carries the human-readable reason (undecodable file, silent or
    /// too-short IR, unknown instance).
    BuiltinIrResult {
        plugin_instance_id: String,
        ok: bool,
        /// Display name echoed from the request.
        name: String,
        #[serde(default)]
        error: Option<String>,
        /// Frames actually convolved, at the engine's rate (0 on failure).
        #[serde(default)]
        frames: u64,
        /// Latency the convolution adds, in samples (0 on failure).
        #[serde(default)]
        latency_samples: u64,
        /// The file carried two distinct channels (true-stereo IR).
        #[serde(default)]
        stereo: bool,
        /// The file was longer than the engine's cap and got cut.
        #[serde(default)]
        truncated: bool,
    },
    /// The user operated a control in a host-drawn chrome strip
    /// ([`HostCommand::SetEditorChrome`]).
    ///
    /// The host neither applies nor interprets it: the insert, the engine and
    /// the preset files are the studio's, and this is the same queue the GPUI
    /// chrome puts its own presses on.
    EditorChromeAction {
        plugin_instance_id: String,
        action: EditorChromeCommand,
    },
}

impl EditorChromeCommand {
    /// The wire discriminant a host-drawn strip queues for this command.
    ///
    /// The strip is Objective-C and cannot name this enum, so it queues an
    /// integer and the host turns it back. Append only: a renumbering would
    /// make a tab close when the user meant to select it, silently and only on
    /// the platform that draws its own chrome.
    pub const fn wire_kind(&self) -> i32 {
        match self {
            Self::SetActive { .. } => 0,
            Self::StepPreset { .. } => 1,
            Self::SavePreset => 2,
            Self::SelectPreset { .. } => 3,
            Self::SelectTab { .. } => 4,
            Self::CloseTab { .. } => 5,
        }
    }

    /// Rebuild one command from what a host-drawn strip queued.
    ///
    /// `insert_id` is empty for every control that is not per-tab, and an
    /// unrecognised or malformed press is `None` rather than a guess: a press
    /// this side does not understand is one the studio must not act on.
    pub fn from_wire(kind: i32, value: i32, insert_id: String) -> Option<Self> {
        match kind {
            0 => Some(Self::SetActive { active: value != 0 }),
            1 => Some(Self::StepPreset { delta: value }),
            2 => Some(Self::SavePreset),
            3 => u32::try_from(value)
                .ok()
                .map(|index| Self::SelectPreset { index }),
            4 => (!insert_id.is_empty()).then_some(Self::SelectTab { insert_id }),
            5 => (!insert_id.is_empty()).then_some(Self::CloseTab { insert_id }),
            _ => None,
        }
    }
}

/// One VST3 parameter entry returned by the plugin host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPluginParameter {
    pub id: u32,
    pub title: String,
    #[serde(default)]
    pub short_title: String,
    #[serde(default)]
    pub unit: String,
    pub automatable: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub read_only: bool,
}

fn default_editor_resizable() -> bool {
    true
}

/// Serialize `msg` as a single JSON line (object + `\n`) and flush.
pub fn write_frame<W: Write>(writer: &mut W, msg: &impl Serialize) -> io::Result<()> {
    let line = serde_json::to_string(msg).map_err(io::Error::other)?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Maximum size of a single newline-delimited frame. Frames carry base64
/// plugin-state blobs, so the cap is generous — but bounded so a stuck or
/// hostile peer cannot drive unbounded memory growth with one giant,
/// newline-less line.
pub const MAX_FRAME_BYTES: u64 = 128 * 1024 * 1024;

/// Read one newline-delimited JSON frame, skipping blank lines.
///
/// Returns `Ok(None)` on EOF (the peer closed the pipe — for the client this
/// means the host exited/crashed; for the host it means the main app is gone).
pub fn read_frame<T: DeserializeOwned, R: BufRead>(reader: &mut R) -> io::Result<Option<T>> {
    read_frame_limited(reader, MAX_FRAME_BYTES)
}

/// [`read_frame`] with an explicit per-frame byte ceiling. Split out so the
/// bound is unit-testable without allocating the production-size limit.
fn read_frame_limited<T: DeserializeOwned, R: BufRead>(
    reader: &mut R,
    max_bytes: u64,
) -> io::Result<Option<T>> {
    let mut line = String::new();
    loop {
        line.clear();
        // Bound the read so a frame without a trailing newline cannot grow
        // `line` past the ceiling. `take` caps this single `read_line` call.
        let read = reader.by_ref().take(max_bytes + 1).read_line(&mut line)?;
        if read == 0 {
            return Ok(None); // EOF
        }
        if read as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC frame exceeds maximum size",
            ));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg = serde_json::from_str::<T>(trimmed).map_err(io::Error::other)?;
        return Ok(Some(msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn command_round_trips_through_frame() {
        let cmd = HostCommand::LoadPlugin {
            plugin_instance_id: "track1:insert2".into(),
            plugin_path: "C:/VST3/Example.vst3".into(),
            class_id: "ABCDEF0123456789".into(),
            sample_rate: 48_000,
            max_block_size: 256,
            format: Some("VST3".into()),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &cmd).unwrap();
        assert!(buf.ends_with(b"\n"));

        let mut reader = Cursor::new(buf);
        let decoded: HostCommand = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(decoded, cmd);
    }

    #[test]
    fn builtin_load_round_trips_through_frame() {
        let cmd = HostCommand::LoadBuiltinPlugin {
            plugin_instance_id: "track1:insert3".into(),
            plugin_id: "rodharerist".into(),
            sample_rate: 48_000,
            max_block_size: 256,
            state_json: Some("{\"schema_version\":3}".into()),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &cmd).unwrap();
        let mut reader = Cursor::new(buf);
        assert_eq!(
            read_frame::<HostCommand, _>(&mut reader).unwrap(),
            Some(cmd)
        );
    }

    /// IR payloads are base64 inside a newline-framed JSON line — the one
    /// place a raw byte could break the framing if the encoding regressed.
    #[test]
    fn builtin_ir_load_and_result_round_trip_through_frames() {
        let cmd = HostCommand::LoadBuiltinIr {
            plugin_instance_id: "track1:insert3".into(),
            name: "Vintage 4x12 SM57.wav".into(),
            // Base64 of bytes that include a newline and other framing-hostile
            // values, proving the encoding keeps the line intact.
            wav_b64: "UklGRgoAAABXQVZF\ngAB/AA==".replace('\n', ""),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &cmd).unwrap();
        assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1);
        let mut reader = Cursor::new(buf);
        assert_eq!(
            read_frame::<HostCommand, _>(&mut reader).unwrap(),
            Some(cmd)
        );

        let ev = HostEvent::BuiltinIrResult {
            plugin_instance_id: "track1:insert3".into(),
            ok: true,
            name: "Vintage 4x12 SM57.wav".into(),
            error: None,
            frames: 4_800,
            latency_samples: 128,
            stereo: false,
            truncated: false,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &ev).unwrap();
        let mut reader = Cursor::new(buf);
        assert_eq!(read_frame::<HostEvent, _>(&mut reader).unwrap(), Some(ev));
    }

    /// The transport key event carries no instance and must stay decodable from
    /// an older host that predates the `source` and `time_ms` fields.
    #[test]
    fn transport_toggle_event_round_trips_and_tolerates_a_missing_source() {
        let ev = HostEvent::TransportToggleRequested {
            source: "editor".into(),
            time_ms: Some(1_234_567),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &ev).unwrap();
        let mut reader = Cursor::new(buf);
        assert_eq!(read_frame::<HostEvent, _>(&mut reader).unwrap(), Some(ev));

        let mut legacy = Cursor::new(b"{\"event\":\"TransportToggleRequested\"}\n".to_vec());
        assert_eq!(
            read_frame::<HostEvent, _>(&mut legacy).unwrap(),
            Some(HostEvent::TransportToggleRequested {
                source: String::new(),
                time_ms: None
            })
        );
    }

    #[test]
    fn event_round_trips_and_skips_blank_lines() {
        let ev = HostEvent::EditorAttached {
            plugin_instance_id: "track1:insert2".into(),
            result: 0,
            preferred_width: 1236,
            preferred_height: 736,
            resizable: true,
            host_hwnd: 0,
        };
        let mut buf = Vec::new();
        // Leading blank lines must be tolerated.
        buf.extend_from_slice(b"\n\n");
        write_frame(&mut buf, &ev).unwrap();

        let mut reader = Cursor::new(buf);
        let decoded: HostEvent = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(decoded, ev);
    }

    #[test]
    fn read_frame_returns_none_on_eof() {
        let mut reader = Cursor::new(Vec::new());
        let decoded: Option<HostCommand> = read_frame(&mut reader).unwrap();
        assert!(decoded.is_none());
    }

    #[test]
    fn read_frame_rejects_oversized_line() {
        // A line longer than the ceiling must error instead of growing the
        // buffer without bound.
        let line = format!("{}\n", "x".repeat(64));
        let mut reader = Cursor::new(line.into_bytes());
        let result: io::Result<Option<HostCommand>> = read_frame_limited(&mut reader, 16);
        assert!(result.is_err(), "oversized frame must be rejected");
    }

    #[test]
    fn read_frame_limited_accepts_within_limit() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &HostCommand::Shutdown).unwrap();
        let mut reader = Cursor::new(buf);
        let decoded: Option<HostCommand> =
            read_frame_limited(&mut reader, MAX_FRAME_BYTES).unwrap();
        assert_eq!(decoded, Some(HostCommand::Shutdown));
    }

    #[test]
    fn tagged_representation_is_stable() {
        let json = serde_json::to_string(&HostCommand::Shutdown).unwrap();
        assert_eq!(json, r#"{"cmd":"Shutdown"}"#);
        let json = serde_json::to_string(&HostEvent::Ready {
            protocol_version: PROTOCOL_VERSION,
            pid: 42,
        })
        .unwrap();
        // Built from the constant on purpose: this test pins the tag and field
        // names, not the version number, which is meant to move.
        assert_eq!(
            json,
            format!(r#"{{"event":"Ready","protocol_version":{PROTOCOL_VERSION},"pid":42}}"#)
        );
    }

    /// The one contract a host-drawn chrome strip cannot express in Rust.
    ///
    /// `editor_mac_shell.mm` queues these integers by hand; if the two tables
    /// ever disagree, a press does something other than what it says — a tab
    /// closes instead of selecting, or a preset step becomes a bypass. Pinning
    /// the numbers here is what makes a renumbering fail a test rather than a
    /// user's session.
    #[test]
    fn chrome_action_wire_numbers_are_pinned() {
        for (kind, command) in [
            (0, EditorChromeCommand::SetActive { active: true }),
            (1, EditorChromeCommand::StepPreset { delta: -1 }),
            (2, EditorChromeCommand::SavePreset),
            (3, EditorChromeCommand::SelectPreset { index: 7 }),
            (
                4,
                EditorChromeCommand::SelectTab {
                    insert_id: "insert-2".to_string(),
                },
            ),
            (
                5,
                EditorChromeCommand::CloseTab {
                    insert_id: "insert-2".to_string(),
                },
            ),
        ] {
            assert_eq!(command.wire_kind(), kind, "{command:?} moved");
        }
    }

    #[test]
    fn a_chrome_press_survives_the_round_trip() {
        let cases = [
            EditorChromeCommand::SetActive { active: false },
            EditorChromeCommand::SetActive { active: true },
            EditorChromeCommand::StepPreset { delta: 1 },
            EditorChromeCommand::StepPreset { delta: -1 },
            EditorChromeCommand::SavePreset,
            EditorChromeCommand::SelectPreset { index: 0 },
            EditorChromeCommand::SelectPreset { index: 12 },
            EditorChromeCommand::SelectTab {
                insert_id: "track1:insert3".to_string(),
            },
            EditorChromeCommand::CloseTab {
                insert_id: "track1:insert3".to_string(),
            },
        ];
        for command in cases {
            let (value, id) = match &command {
                EditorChromeCommand::SetActive { active } => (i32::from(*active), String::new()),
                EditorChromeCommand::StepPreset { delta } => (*delta, String::new()),
                EditorChromeCommand::SavePreset => (0, String::new()),
                EditorChromeCommand::SelectPreset { index } => (*index as i32, String::new()),
                EditorChromeCommand::SelectTab { insert_id }
                | EditorChromeCommand::CloseTab { insert_id } => (0, insert_id.clone()),
            };
            assert_eq!(
                EditorChromeCommand::from_wire(command.wire_kind(), value, id),
                Some(command.clone()),
                "{command:?} did not survive the wire"
            );
        }
    }

    /// A press this side does not understand must not reach the studio: it
    /// would be applied as *something*, and the wrong something is worse than
    /// nothing happening.
    #[test]
    fn a_malformed_chrome_press_is_dropped_not_guessed() {
        assert_eq!(EditorChromeCommand::from_wire(99, 0, String::new()), None);
        assert_eq!(EditorChromeCommand::from_wire(-1, 0, String::new()), None);
        // A negative preset row is not row zero.
        assert_eq!(EditorChromeCommand::from_wire(3, -1, String::new()), None);
        // A tab action with no tab names nothing to act on.
        assert_eq!(EditorChromeCommand::from_wire(4, 0, String::new()), None);
        assert_eq!(EditorChromeCommand::from_wire(5, 0, String::new()), None);
    }

    /// The chrome command has to survive the transport it was added to, and its
    /// palette has to arrive as the same numbers that went in — a colour that
    /// silently round-trips wrong is a strip that does not match the theme.
    #[test]
    fn chrome_state_survives_the_transport() {
        let command = HostCommand::SetEditorChrome {
            plugin_instance_id: "track1:insert1".to_string(),
            title: "Drums - Pro-Q 4".to_string(),
            active: true,
            cpu_label: "12%".to_string(),
            latency_label: "3.2 ms".to_string(),
            preset_label: "Vocal Bus".to_string(),
            presets: vec!["Vocal Bus".to_string(), "Drum Glue".to_string()],
            preset_index: Some(0),
            tabs: vec![EditorChromeTab {
                insert_id: "track1:insert1".to_string(),
                display_name: "Pro-Q 4".to_string(),
                insert_number: 1,
            }],
            active_tab: "track1:insert1".to_string(),
            palette: EditorChromePalette {
                strip_bg: 0x1B1D22FF,
                row_bg: 0x212429FF,
                border: 0x00000033,
                control_bg: 0x16181CFF,
                control_hover: 0x2A2E35FF,
                control_pressed: 0x101216FF,
                accent: 0x4FC9D8FF,
                text_primary: 0xE8EAEEFF,
                text_secondary: 0xB9BDC6FF,
                text_faint: 0x8F949FFF,
            },
        };
        let line = serde_json::to_string(&command).unwrap();
        let decoded: HostCommand = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded, command);
    }
}
