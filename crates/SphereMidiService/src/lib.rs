//! Futureboard MIDI service layer.
//!
//! This crate owns MIDI data types, MIDI device enumeration, preference merge,
//! and UI/control-path MIDI event primitives. UI crates should depend on this
//! crate instead of carrying MIDI service state internally.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub mod expression;
pub mod mpe;
pub mod sysex;

pub use expression::{
    CustomExpressionLane, ExpressionCurve, ExpressionInterpolation, ExpressionPoint,
    ExpressionSimplificationTolerances, NoteExpression, NoteExpressionLane, NoteId,
    PitchExpressionConfig, Tick, simplify_expression_curve,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MidiDeviceDirection {
    Input,
    Output,
    InputOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MidiDeviceSetting {
    pub id: String,
    pub name: String,
    pub direction: MidiDeviceDirection,
    pub enabled: bool,
    pub connected: bool,
    #[serde(default)]
    pub clock_enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MidiHardwareSettings {
    #[serde(default)]
    pub devices: Vec<MidiDeviceSetting>,
    pub clock_sync: bool,
    /// Legacy — migrated into [`devices`] on load.
    #[serde(default, skip_serializing)]
    pub enabled_inputs: Vec<String>,
    #[serde(default, skip_serializing)]
    pub enabled_outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedMidiDevice {
    pub id: String,
    pub name: String,
    pub direction: MidiDeviceDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiInputSource {
    Hardware,
    PianoRollPreview,
    VirtualKeyboard,
    DawRemote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualKeyboardEvent {
    NoteOn { note: u8, velocity: u8, channel: u8 },
    NoteOff { note: u8, channel: u8 },
    Sustain { down: bool, channel: u8 },
    Panic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiInputEvent {
    NoteOn {
        note: u8,
        velocity: u8,
        channel: u8,
    },
    NoteOff {
        note: u8,
        channel: u8,
    },
    ControlChange {
        controller: u8,
        value: u8,
        channel: u8,
    },
    PitchBend {
        /// 14-bit unsigned MIDI value, where 8192 is centre.
        value: u16,
        channel: u8,
    },
    ChannelPressure {
        value: u8,
        channel: u8,
    },
    PolyPressure {
        note: u8,
        value: u8,
        channel: u8,
    },
    AllNotesOff,
    Panic,
}

impl From<VirtualKeyboardEvent> for MidiInputEvent {
    fn from(event: VirtualKeyboardEvent) -> Self {
        match event {
            VirtualKeyboardEvent::NoteOn {
                note,
                velocity,
                channel,
            } => Self::NoteOn {
                note,
                velocity,
                channel,
            },
            VirtualKeyboardEvent::NoteOff { note, channel } => Self::NoteOff { note, channel },
            VirtualKeyboardEvent::Sustain { down, channel } => Self::ControlChange {
                controller: 64,
                value: if down { 127 } else { 0 },
                channel,
            },
            VirtualKeyboardEvent::Panic => Self::Panic,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiInputTarget {
    pub track_id: String,
    pub plugin_instance_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiInputRouteStatus {
    Routed,
    NoTarget,
    EngineUnavailable,
    DispatchFailed(String),
}

pub struct MidiInputRouter;

impl MidiInputRouter {
    pub fn sanitize_channel(channel: u8) -> u8 {
        channel.min(15)
    }

    pub fn sanitize_note(note: u8) -> u8 {
        note.min(127)
    }

    pub fn sanitize_velocity(velocity: u8) -> u8 {
        velocity.clamp(1, 127)
    }
}

pub fn midi_settings_debug_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_MIDI_SETTINGS_DEBUG").is_some())
}

#[cfg(target_os = "macos")]
mod macos_coremidi;

/// Strip the volatile ALSA sequencer address from a port name.
///
/// On Linux, midir reports ports as `"Client:Port <client_id>:<port_id>"`, e.g.
/// `"Launchkey Mini:Launchkey Mini MIDI 1 24:0"`. Those numbers are assigned by
/// the sequencer at connect time and change whenever the device is replugged or
/// the machine reboots.
///
/// Leaving them in is not cosmetic: [`stable_id`] slugs the name, so the id
/// would change with the address and a saved device would come back as a new
/// one — losing its enabled state and leaving a phantom disconnected row behind.
/// Stripping the address makes the identity stable across sessions.
///
/// Windows and macOS never produce this shape, so this is a no-op there. Only a
/// trailing ` <digits>:<digits>` is removed; a name that merely *contains* a
/// colon (which both other platforms do) is left alone.
pub(crate) fn normalize_port_name(name: &str) -> String {
    let trimmed = name.trim_end();
    let Some((head, tail)) = trimmed.rsplit_once(' ') else {
        return trimmed.to_string();
    };
    let Some((client, port)) = tail.split_once(':') else {
        return trimmed.to_string();
    };
    let is_address = !client.is_empty()
        && !port.is_empty()
        && client.bytes().all(|b| b.is_ascii_digit())
        && port.bytes().all(|b| b.is_ascii_digit());
    if !is_address {
        return trimmed.to_string();
    }
    let head = head.trim_end();
    // Never normalize a name down to nothing — an address-only port keeps the
    // only text it has rather than becoming unnameable.
    if head.is_empty() {
        trimmed.to_string()
    } else {
        head.to_string()
    }
}

pub(crate) fn stable_id(direction: MidiDeviceDirection, name: &str) -> String {
    let slug = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let prefix = match direction {
        MidiDeviceDirection::Input => "midi-in",
        MidiDeviceDirection::Output => "midi-out",
        MidiDeviceDirection::InputOutput => "midi-io",
    };
    format!("{prefix}-{slug}")
}

/// One-based occurrence encoded by duplicate-port ids (`...-2`, `...-3`).
/// Accept every direction prefix because bidirectional devices are coalesced
/// to `midi-io-*` but are opened through the input/output-specific backend.
pub(crate) fn stable_id_ordinal(device_id: &str, name: &str) -> usize {
    for direction in [
        MidiDeviceDirection::InputOutput,
        MidiDeviceDirection::Input,
        MidiDeviceDirection::Output,
    ] {
        let base = stable_id(direction, name);
        if device_id == base {
            return 1;
        }
        if let Some(suffix) = device_id.strip_prefix(&format!("{base}-")) {
            if let Ok(ordinal) = suffix.parse::<usize>() {
                return ordinal.max(1);
            }
        }
    }
    1
}

/// True when the platform reported a MIDI port added or removed since the last
/// call, and clears the flag.
///
/// A *hint* for invalidating a cached device list, never the list itself: only
/// macOS currently pushes notifications (via the CoreMIDI client's notify proc),
/// so Windows and Linux always answer `false` and keep relying on an explicit
/// rescan. Callers must therefore treat `false` as "nothing new to report",
/// not as "the cache is definitely current".
pub fn take_midi_ports_changed() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_coremidi::take_ports_changed()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Real MIDI port scan. Windows/Linux use `midir`; macOS uses a thin CoreMIDI
/// FFI path (avoids midir/coremidi vs gpui `core-foundation` pin conflict).
/// Enumeration only reads port names — it never opens the hardware.
/// Wrapped in `catch_unwind` so a misbehaving backend yields an empty list and a
/// warning rather than taking down the UI thread.
pub fn scan_midi_ports() -> Vec<DetectedMidiDevice> {
    match std::panic::catch_unwind(real_scan_midi_ports) {
        Ok(devices) => {
            if midi_settings_debug_enabled() {
                eprintln!("[MIDI settings] detected devices ({})", devices.len());
                for device in &devices {
                    eprintln!(
                        "  - {} ({:?}) id={}",
                        device.name, device.direction, device.id
                    );
                }
            }
            devices
        }
        Err(_) => {
            eprintln!("[MidiDeviceScan] enumeration panicked — returning empty list");
            Vec::new()
        }
    }
}

/// macOS: thin CoreMIDI enumeration (avoids midir/coremidi vs gpui CF pin).
#[cfg(target_os = "macos")]
fn real_scan_midi_ports() -> Vec<DetectedMidiDevice> {
    macos_coremidi::scan_ports()
}

#[cfg(not(target_os = "macos"))]
fn real_scan_midi_ports() -> Vec<DetectedMidiDevice> {
    use midir::{MidiInput, MidiOutput};

    let mut devices = Vec::new();
    match MidiInput::new("Futureboard MIDI scan (in)") {
        Ok(input) => {
            for port in input.ports() {
                if let Ok(name) = input.port_name(&port) {
                    // Normalized so a Linux replug does not mint a new identity.
                    let name = normalize_port_name(&name);
                    devices.push(DetectedMidiDevice {
                        id: stable_id(MidiDeviceDirection::Input, &name),
                        name,
                        direction: MidiDeviceDirection::Input,
                    });
                }
            }
        }
        Err(e) => eprintln!("[MidiDeviceScan] MIDI input backend unavailable: {e}"),
    }
    match MidiOutput::new("Futureboard MIDI scan (out)") {
        Ok(output) => {
            for port in output.ports() {
                if let Ok(name) = output.port_name(&port) {
                    let name = normalize_port_name(&name);
                    devices.push(DetectedMidiDevice {
                        id: stable_id(MidiDeviceDirection::Output, &name),
                        name,
                        direction: MidiDeviceDirection::Output,
                    });
                }
            }
        }
        Err(e) => eprintln!("[MidiDeviceScan] MIDI output backend unavailable: {e}"),
    }
    coalesce_detected_midi_devices(devices)
}

/// Merge same-name input and output ports into a single InputOutput entry so a
/// bidirectional controller does not appear twice in Preferences / routing lists.
pub(crate) fn coalesce_detected_midi_devices(
    devices: Vec<DetectedMidiDevice>,
) -> Vec<DetectedMidiDevice> {
    let mut resolved: Vec<DetectedMidiDevice> = Vec::with_capacity(devices.len());
    for device in devices {
        let opposite = match device.direction {
            MidiDeviceDirection::Input => MidiDeviceDirection::Output,
            MidiDeviceDirection::Output => MidiDeviceDirection::Input,
            MidiDeviceDirection::InputOutput => {
                resolved.push(device);
                continue;
            }
        };
        if let Some(existing) = resolved
            .iter_mut()
            .find(|existing| existing.name == device.name && existing.direction == opposite)
        {
            existing.direction = MidiDeviceDirection::InputOutput;
            existing.id = stable_id(MidiDeviceDirection::InputOutput, &existing.name);
        } else {
            resolved.push(device);
        }
    }

    // Multiple physical ports frequently expose the same display name. Keep
    // every endpoint and disambiguate only colliding ids; name-keyed merging
    // used to silently drop all but one port on every platform.
    let mut id_counts: HashMap<String, usize> = HashMap::new();
    for device in &mut resolved {
        let base = if device.id.trim().is_empty() {
            stable_id(device.direction, &device.name)
        } else {
            device.id.clone()
        };
        let count = id_counts.entry(base.clone()).or_insert(0);
        *count += 1;
        device.id = if *count == 1 {
            base
        } else {
            format!("{base}-{}", *count)
        };
    }
    resolved
}

/// Merge saved preferences with freshly detected devices. Saved-only entries stay visible as missing.
pub fn resolve_midi_devices(
    saved: &[MidiDeviceSetting],
    detected: &[DetectedMidiDevice],
) -> Vec<MidiDeviceSetting> {
    let detected = coalesce_detected_midi_devices(detected.to_vec());
    if midi_settings_debug_enabled() {
        eprintln!("[MIDI settings] saved preferences ({})", saved.len());
        for device in saved {
            eprintln!(
                "  - {} enabled={} connected={} clock={}",
                device.name, device.enabled, device.connected, device.clock_enabled
            );
        }
    }

    let saved_by_id: HashMap<&str, &MidiDeviceSetting> =
        saved.iter().map(|d| (d.id.as_str(), d)).collect();
    let saved_by_name: HashMap<&str, &MidiDeviceSetting> =
        saved.iter().map(|d| (d.name.as_str(), d)).collect();
    let mut resolved = Vec::new();
    let mut resolved_names = std::collections::HashSet::new();

    for det in &detected {
        let saved = saved_by_id
            .get(det.id.as_str())
            .copied()
            .or_else(|| saved_by_name.get(det.name.as_str()).copied());
        resolved_names.insert(det.name.clone());
        resolved.push(MidiDeviceSetting {
            id: det.id.clone(),
            name: det.name.clone(),
            direction: det.direction,
            // New devices default to enabled so real-time MIDI input works
            // without a Preferences visit; users can still disable them.
            enabled: saved.map(|s| s.enabled).unwrap_or(true),
            connected: true,
            clock_enabled: saved.map(|s| s.clock_enabled).unwrap_or(false),
        });
    }

    for saved_device in saved {
        if detected.iter().any(|d| d.id == saved_device.id)
            || resolved_names.contains(&saved_device.name)
        {
            continue;
        }
        if midi_settings_debug_enabled() {
            eprintln!(
                "[MIDI settings] missing saved device: {} ({})",
                saved_device.name, saved_device.id
            );
        }
        resolved.push(MidiDeviceSetting {
            id: saved_device.id.clone(),
            name: saved_device.name.clone(),
            direction: saved_device.direction,
            enabled: saved_device.enabled,
            connected: false,
            clock_enabled: saved_device.clock_enabled,
        });
    }

    resolved
}

pub fn upsert_midi_device(midi: &mut MidiHardwareSettings, device: MidiDeviceSetting) {
    if let Some(existing) = midi.devices.iter_mut().find(|d| d.id == device.id) {
        *existing = device;
    } else {
        midi.devices.push(device);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HardwareMidiEvent {
    /// MIDI output device id or display name. The GPUI routing UI currently
    /// stores the display name; matching accepts either for migration safety.
    pub device_id: String,
    /// Legacy/diagnostic relative time. New playback scheduling uses
    /// [`absolute_sample`] against the audio transport timeline.
    pub delay_seconds: f64,
    /// Musical position used for diagnostics and rebuilds after tempo changes.
    pub beat: f64,
    /// Absolute sample position on the same timeline as audio playback.
    pub absolute_sample: u64,
    /// Raw MIDI bytes, e.g. [0x90 | channel, pitch, velocity].
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct HardwareMidiPlaybackConfig {
    pub start_sample: u64,
    pub sample_rate: u32,
    pub lookahead: Duration,
}

impl HardwareMidiPlaybackConfig {
    pub fn new(start_sample: u64, sample_rate: u32) -> Self {
        Self {
            start_sample,
            sample_rate: sample_rate.max(1),
            lookahead: Duration::from_millis(10),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HardwareMidiProfilerSnapshot {
    /// Platform whose scheduler produced this measurement.
    pub platform: &'static str,
    pub events_per_second: u32,
    pub max_jitter_us: u32,
}

pub struct HardwareMidiPlayback {
    cancel_tx: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<JoinHandle<()>>,
    profiler: std::sync::Arc<HardwareMidiProfiler>,
}

impl Default for HardwareMidiPlayback {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareMidiPlayback {
    pub fn new() -> Self {
        Self {
            cancel_tx: None,
            handle: None,
            profiler: std::sync::Arc::new(HardwareMidiProfiler::default()),
        }
    }

    /// Legacy entry point retained for callers that only know relative seconds.
    /// New transport playback should call [`Self::start_at_sample`] so events are
    /// aligned to the same sample timeline as audio.
    pub fn start(&mut self, mut events: Vec<HardwareMidiEvent>) {
        for event in &mut events {
            if event.absolute_sample == 0 && event.delay_seconds > 0.0 {
                event.absolute_sample = seconds_to_samples(event.delay_seconds, 48_000);
            }
        }
        self.start_with_config(events, HardwareMidiPlaybackConfig::new(0, 48_000));
    }

    pub fn start_at_sample(
        &mut self,
        events: Vec<HardwareMidiEvent>,
        start_sample: u64,
        sample_rate: u32,
    ) {
        self.start_with_config(
            events,
            HardwareMidiPlaybackConfig::new(start_sample, sample_rate),
        );
    }

    pub fn start_with_config(
        &mut self,
        mut events: Vec<HardwareMidiEvent>,
        config: HardwareMidiPlaybackConfig,
    ) {
        self.stop();
        self.profiler.reset();
        if events.is_empty() {
            return;
        }
        sort_hardware_midi_events(&mut events);
        events = coalesce_hardware_midi_events(events, config.sample_rate);
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        self.cancel_tx = Some(cancel_tx);
        self.handle = Some(spawn_hardware_midi_thread(
            events,
            config,
            cancel_rx,
            self.profiler.clone(),
        ));
    }

    pub fn profiler_snapshot(&self) -> HardwareMidiProfilerSnapshot {
        self.profiler.snapshot()
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.cancel_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HardwareMidiPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One decoded MIDI message received from an enabled hardware input port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HardwareMidiInputMessage {
    pub device_id: String,
    pub device_name: String,
    pub event: MidiInputEvent,
    /// Monotonic arrival time captured in the native MIDI callback. Consumers
    /// use this to compensate recording placement when the UI thread is busy.
    pub received_at: Instant,
}

/// Control-thread hardware MIDI input listener. Opens midir connections for
/// enabled input / input-output devices and pushes decoded events onto a
/// bounded queue drained by the UI poll loop.
pub struct HardwareMidiInput {
    event_rx: Option<std::sync::mpsc::Receiver<HardwareMidiInputMessage>>,
    /// Cancel + join handles for the active input connections.
    connections: Vec<HardwareMidiInputConnection>,
    /// Last synced set of enabled device ids (so we can no-op when unchanged).
    enabled_ids: Vec<String>,
}

struct HardwareMidiInputConnection {
    #[allow(dead_code)]
    device_id: String,
    /// Dropping the connection closes the port. Kept alive for the session.
    #[cfg(not(target_os = "macos"))]
    _connection: midir::MidiInputConnection<()>,
    #[cfg(target_os = "macos")]
    _connection: macos_coremidi::MacMidiInputConnection,
}

impl Default for HardwareMidiInput {
    fn default() -> Self {
        Self::new()
    }
}

impl HardwareMidiInput {
    pub fn new() -> Self {
        Self {
            event_rx: None,
            connections: Vec::new(),
            enabled_ids: Vec::new(),
        }
    }

    /// Open / refresh connections for every enabled input-capable device.
    /// Safe to call repeatedly from the UI poll; reconnects only when the
    /// enabled set changes.
    pub fn sync_enabled_devices(&mut self, devices: &[MidiDeviceSetting]) {
        let mut enabled: Vec<(String, String)> = devices
            .iter()
            .filter(|d| {
                d.enabled
                    && d.connected
                    && matches!(
                        d.direction,
                        MidiDeviceDirection::Input | MidiDeviceDirection::InputOutput
                    )
            })
            .map(|d| (d.id.clone(), d.name.clone()))
            .collect();
        enabled.sort_by(|a, b| a.0.cmp(&b.0));
        let enabled_ids: Vec<String> = enabled.iter().map(|(id, _)| id.clone()).collect();
        if enabled_ids == self.enabled_ids {
            return;
        }
        self.stop();
        self.enabled_ids = enabled_ids;
        if enabled.is_empty() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.event_rx = Some(rx);
        self.connections = open_hardware_midi_inputs(enabled, tx);
        if self.connections.is_empty() {
            // Nothing opened: the platform backend may still be coming up (on
            // macOS the CoreMIDI client is retried until it succeeds). Hold back
            // the comparison key so the next sync tries again instead of
            // treating this enabled set as done. Only when *nothing* connected,
            // so a device that did open is never torn down to retry a sibling.
            self.enabled_ids.clear();
        }
    }

    /// Drain pending hardware MIDI messages (non-blocking). Bounded by caller.
    pub fn drain(&mut self, max: usize) -> Vec<HardwareMidiInputMessage> {
        let Some(rx) = self.event_rx.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while out.len() < max {
            match rx.try_recv() {
                Ok(msg) => out.push(msg),
                Err(_) => break,
            }
        }
        out
    }

    pub fn stop(&mut self) {
        self.connections.clear();
        self.event_rx = None;
        self.enabled_ids.clear();
    }
}

impl Drop for HardwareMidiInput {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) fn decode_midi_bytes(bytes: &[u8]) -> Option<MidiInputEvent> {
    if bytes.is_empty() {
        return None;
    }
    let status = bytes[0];
    let kind = status & 0xF0;
    let channel = status & 0x0F;
    match kind {
        0x90 => {
            let note = *bytes.get(1)?;
            let velocity = *bytes.get(2)?;
            if velocity == 0 {
                Some(MidiInputEvent::NoteOff { note, channel })
            } else {
                Some(MidiInputEvent::NoteOn {
                    note,
                    velocity,
                    channel,
                })
            }
        }
        0x80 => {
            let note = *bytes.get(1)?;
            // A channel Note Off always carries release velocity, even though
            // the legacy event type does not expose it yet.
            let _release_velocity = *bytes.get(2)?;
            Some(MidiInputEvent::NoteOff { note, channel })
        }
        0xB0 => {
            let controller = *bytes.get(1)?;
            let value = *bytes.get(2)?;
            if controller == 123 {
                Some(MidiInputEvent::AllNotesOff)
            } else {
                Some(MidiInputEvent::ControlChange {
                    controller,
                    value,
                    channel,
                })
            }
        }
        0xA0 => Some(MidiInputEvent::PolyPressure {
            note: *bytes.get(1)?,
            value: *bytes.get(2)?,
            channel,
        }),
        0xD0 => Some(MidiInputEvent::ChannelPressure {
            value: *bytes.get(1)?,
            channel,
        }),
        0xE0 => Some(MidiInputEvent::PitchBend {
            value: u16::from(*bytes.get(1)?) | (u16::from(*bytes.get(2)?) << 7),
            channel,
        }),
        _ => None,
    }
}

/// Number of data bytes that follow `status` in a MIDI 1.0 stream.
fn midi_data_byte_count(status: u8) -> usize {
    match status & 0xF0 {
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => 2,
        0xC0 | 0xD0 => 1,
        0xF0 => match status {
            // MTC quarter frame, song select.
            0xF1 | 0xF3 => 1,
            // Song position pointer.
            0xF2 => 2,
            _ => 0,
        },
        _ => 0,
    }
}

/// Split a raw MIDI byte stream into complete messages, calling `on_message`
/// once per message. Returns the number of bytes that could not be framed.
///
/// `midir` (Windows/Linux) hands the app exactly one message per callback, but a
/// CoreMIDI packet is a *stream*: messages that share a timestamp are coalesced
/// into a single packet, and some class-compliant drivers pass running status
/// through. Decoding only the first three bytes of a packet therefore drops
/// every message after the first — a chord arrives as one note.
///
/// `running_status` is threaded through by the caller because a packet boundary
/// does not reset it. Per MIDI 1.0, System Real-Time bytes are single-byte
/// messages that may appear anywhere and never disturb running status, while
/// System Common clears it.
pub fn split_midi_messages(
    bytes: &[u8],
    running_status: &mut Option<u8>,
    mut on_message: impl FnMut(&[u8]),
) -> usize {
    let mut malformed = 0usize;
    let mut message = [0u8; 3];
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];

        // System Real-Time: one byte, legal anywhere, leaves running status alone.
        if byte >= 0xF8 {
            on_message(&bytes[index..index + 1]);
            index += 1;
            continue;
        }

        // SysEx runs to EOX. We do not consume dumps, so the span is emitted
        // whole (real-time bytes inside it included) for the decoder to ignore.
        if byte == 0xF0 {
            *running_status = None;
            let mut end = index + 1;
            while end < bytes.len() {
                let byte = bytes[end];
                if byte == 0xF7 {
                    end += 1;
                    break;
                }
                // A non-real-time status byte means the dump was truncated.
                if byte >= 0x80 && byte < 0xF8 {
                    break;
                }
                end += 1;
            }
            on_message(&bytes[index..end]);
            index = end;
            continue;
        }

        let (status, data_start) = if byte >= 0x80 {
            if byte >= 0xF0 {
                *running_status = None;
            } else {
                *running_status = Some(byte);
            }
            (byte, index + 1)
        } else if let Some(status) = *running_status {
            (status, index)
        } else {
            // A data byte with nothing to attach it to: the stream started
            // mid-message. Drop it rather than guessing at a status.
            malformed += 1;
            index += 1;
            continue;
        };

        let needed = midi_data_byte_count(status);
        let end = data_start + needed;
        if end > bytes.len() {
            // Truncated tail. Neither backend splits a message across
            // callbacks, so this is a malformed stream, not something to buffer.
            malformed += bytes.len() - index;
            break;
        }
        message[0] = status;
        message[1..=needed].copy_from_slice(&bytes[data_start..end]);
        on_message(&message[..=needed]);
        index = end;
    }

    malformed
}

/// Counters for the hardware MIDI input boundary, so a silent keyboard can be
/// diagnosed without a debugger: they separate "the backend never delivered
/// anything" from "bytes arrived but did not decode" from "events decoded but
/// no track claimed them".
///
/// Plain relaxed atomics — the native MIDI callback increments them without
/// locking, allocating, or blocking, and readers only ever print them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MidiInputDiagnostics {
    /// Native callbacks delivered by the platform backend.
    pub callbacks: u64,
    /// Complete MIDI messages framed out of those callbacks.
    pub messages: u64,
    /// Messages that became a [`MidiInputEvent`].
    pub events: u64,
    /// Messages deliberately not mapped (clock, sysex, aftertouch, ...).
    pub ignored: u64,
    /// Bytes that could not be framed into a message.
    pub malformed: u64,
    /// Events dropped because the queue receiver was gone.
    pub dropped: u64,
    /// Events the router delivered to at least one track.
    pub routed: u64,
    /// Events the router discarded because no track claimed them.
    pub unrouted: u64,
}

mod input_counters {
    use std::sync::atomic::AtomicU64;

    pub(super) static CALLBACKS: AtomicU64 = AtomicU64::new(0);
    pub(super) static MESSAGES: AtomicU64 = AtomicU64::new(0);
    pub(super) static EVENTS: AtomicU64 = AtomicU64::new(0);
    pub(super) static IGNORED: AtomicU64 = AtomicU64::new(0);
    pub(super) static MALFORMED: AtomicU64 = AtomicU64::new(0);
    pub(super) static DROPPED: AtomicU64 = AtomicU64::new(0);
    pub(super) static ROUTED: AtomicU64 = AtomicU64::new(0);
    pub(super) static UNROUTED: AtomicU64 = AtomicU64::new(0);
}

use std::sync::atomic::Ordering as AtomicOrdering;

#[inline]
fn bump(counter: &std::sync::atomic::AtomicU64, by: u64) {
    if by > 0 {
        counter.fetch_add(by, AtomicOrdering::Relaxed);
    }
}

/// Record one native MIDI callback and what came out of it. Called from the
/// platform MIDI thread, so it must stay allocation- and lock-free.
#[inline]
pub(crate) fn note_midi_input_callback(
    messages: u64,
    events: u64,
    ignored: u64,
    malformed: u64,
    dropped: u64,
) {
    bump(&input_counters::CALLBACKS, 1);
    bump(&input_counters::MESSAGES, messages);
    bump(&input_counters::EVENTS, events);
    bump(&input_counters::IGNORED, ignored);
    bump(&input_counters::MALFORMED, malformed);
    bump(&input_counters::DROPPED, dropped);
}

/// Record the routing verdict for one drained input event.
#[inline]
pub fn note_midi_input_routed(routed: bool) {
    if routed {
        bump(&input_counters::ROUTED, 1);
    } else {
        bump(&input_counters::UNROUTED, 1);
    }
}

/// Snapshot of the hardware MIDI input counters.
pub fn midi_input_diagnostics() -> MidiInputDiagnostics {
    MidiInputDiagnostics {
        callbacks: input_counters::CALLBACKS.load(AtomicOrdering::Relaxed),
        messages: input_counters::MESSAGES.load(AtomicOrdering::Relaxed),
        events: input_counters::EVENTS.load(AtomicOrdering::Relaxed),
        ignored: input_counters::IGNORED.load(AtomicOrdering::Relaxed),
        malformed: input_counters::MALFORMED.load(AtomicOrdering::Relaxed),
        dropped: input_counters::DROPPED.load(AtomicOrdering::Relaxed),
        routed: input_counters::ROUTED.load(AtomicOrdering::Relaxed),
        unrouted: input_counters::UNROUTED.load(AtomicOrdering::Relaxed),
    }
}

#[cfg(not(target_os = "macos"))]
fn open_hardware_midi_inputs(
    enabled: Vec<(String, String)>,
    tx: std::sync::mpsc::Sender<HardwareMidiInputMessage>,
) -> Vec<HardwareMidiInputConnection> {
    use midir::MidiInput;

    let mut connections = Vec::new();
    let Ok(scanner) = MidiInput::new("Futureboard MIDI input scan") else {
        eprintln!("[MIDI input] backend unavailable — cannot open hardware ports");
        return connections;
    };
    let ports = scanner.ports();
    for (device_id, device_name) in enabled {
        let ordinal = stable_id_ordinal(&device_id, &device_name);
        let Some(port) = ports
            .iter()
            .filter(|port| {
                scanner
                    .port_name(port)
                    .ok()
                    .map(|name| normalize_port_name(&name))
                    .is_some_and(|name| name == device_name)
            })
            .nth(ordinal - 1)
        else {
            eprintln!("[MIDI input] port not found for enabled device '{device_name}'");
            continue;
        };
        // midir requires a fresh MidiInput per connection.
        let Ok(input) = MidiInput::new(&format!("Futureboard MIDI in ({device_name})")) else {
            continue;
        };
        let tx = tx.clone();
        let id = device_id.clone();
        let name = device_name.clone();
        match input.connect(
            port,
            &format!("Futureboard listen ({device_name})"),
            move |_stamp, message, _| {
                // midir delivers exactly one complete message per callback, so
                // this stays a direct decode; only the diagnostic counters are
                // shared with the CoreMIDI path.
                match decode_midi_bytes(message) {
                    Some(event) => {
                        let sent = tx
                            .send(HardwareMidiInputMessage {
                                device_id: id.clone(),
                                device_name: name.clone(),
                                event,
                                received_at: Instant::now(),
                            })
                            .is_ok();
                        note_midi_input_callback(1, 1, 0, 0, u64::from(!sent));
                    }
                    None => note_midi_input_callback(1, 0, 1, 0, 0),
                }
            },
            (),
        ) {
            Ok(connection) => {
                if midi_settings_debug_enabled() {
                    eprintln!("[MIDI input] connected '{device_name}' ({device_id})");
                }
                connections.push(HardwareMidiInputConnection {
                    device_id,
                    _connection: connection,
                });
            }
            Err(error) => {
                eprintln!("[MIDI input] failed to open '{device_name}': {error}");
            }
        }
    }
    connections
}

#[cfg(target_os = "macos")]
fn open_hardware_midi_inputs(
    enabled: Vec<(String, String)>,
    tx: std::sync::mpsc::Sender<HardwareMidiInputMessage>,
) -> Vec<HardwareMidiInputConnection> {
    macos_coremidi::open_inputs(enabled, tx)
        .into_iter()
        .map(|(device_id, connection)| HardwareMidiInputConnection {
            device_id,
            _connection: connection,
        })
        .collect()
}

fn spawn_hardware_midi_thread(
    events: Vec<HardwareMidiEvent>,
    config: HardwareMidiPlaybackConfig,
    cancel_rx: std::sync::mpsc::Receiver<()>,
    profiler: std::sync::Arc<HardwareMidiProfiler>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("Futureboard MIDI output".to_string())
        .spawn(move || run_hardware_midi_thread(events, config, cancel_rx, profiler))
        .expect("spawn Futureboard MIDI output thread")
}

fn run_hardware_midi_thread(
    events: Vec<HardwareMidiEvent>,
    config: HardwareMidiPlaybackConfig,
    cancel_rx: std::sync::mpsc::Receiver<()>,
    profiler: std::sync::Arc<HardwareMidiProfiler>,
) {
    let _thread_scope = MidiThreadScope::enter();
    let debug = midi_output_debug_enabled();
    let lateness_warnings = midi_lateness_warnings_enabled();
    let sample_rate = config.sample_rate.max(1) as f64;
    let start_sample = config.start_sample;
    let wall_start = Instant::now();
    let lookahead_samples = seconds_to_samples(config.lookahead.as_secs_f64(), config.sample_rate);
    let mut connections: HashMap<String, MidiOutputConnection> = HashMap::new();
    let mut cursor = events.partition_point(|ev| ev.absolute_sample < start_sample);

    // Open enabled target devices once on the MIDI thread. Avoiding open/close
    // during playback prevents WinMM/midir/CoreMIDI from introducing UI-sized stalls.
    for device_id in unique_event_devices(&events[cursor..]) {
        if let Some(conn) = open_midi_output(&device_id) {
            connections.insert(device_id, conn);
        }
    }

    while cursor < events.len() {
        if cancel_rx.try_recv().is_ok() {
            send_all_notes_off(&mut connections);
            return;
        }

        let timeline_sample = start_sample.saturating_add(seconds_to_samples(
            wall_start.elapsed().as_secs_f64(),
            config.sample_rate,
        ));
        let horizon = timeline_sample.saturating_add(lookahead_samples.max(1));

        if events[cursor].absolute_sample > horizon {
            wait_for_midi_tick(Duration::from_millis(1));
            continue;
        }

        let event = &events[cursor];
        let scheduled_wall = wall_start
            + samples_to_duration(
                event.absolute_sample.saturating_sub(start_sample),
                sample_rate,
            );
        while Instant::now() < scheduled_wall {
            if cancel_rx.try_recv().is_ok() {
                send_all_notes_off(&mut connections);
                return;
            }
            let remaining = scheduled_wall.saturating_duration_since(Instant::now());
            wait_for_midi_tick(remaining.min(Duration::from_millis(1)));
        }

        let actual = Instant::now();
        let lateness = actual.saturating_duration_since(scheduled_wall);
        profiler.record(lateness);
        if let Some(conn) = connections.get_mut(&event.device_id) {
            let _ = conn.send(&event.message);
        }
        log_midi_dispatch(
            event,
            scheduled_wall,
            actual,
            lateness,
            debug,
            lateness_warnings,
        );
        cursor += 1;
    }
}

#[cfg(not(target_os = "macos"))]
type MidiOutputConnection = midir::MidiOutputConnection;
#[cfg(target_os = "macos")]
type MidiOutputConnection = macos_coremidi::MacMidiOutputConnection;

fn open_midi_output(device_id_or_name: &str) -> Option<MidiOutputConnection> {
    #[cfg(not(target_os = "macos"))]
    {
        let midi_out = midir::MidiOutput::new("Futureboard MIDI playback").ok()?;
        let ports = midi_out.ports();
        for port in &ports {
            let Ok(raw_name) = midi_out.port_name(&port) else {
                continue;
            };
            // Match on the same normalized name the scan published, or a saved
            // Linux id would never resolve after the sequencer address moved.
            let name = normalize_port_name(&raw_name);
            let stable = stable_id(MidiDeviceDirection::Output, &name);
            let stable_io = stable_id(MidiDeviceDirection::InputOutput, &name);
            if name == device_id_or_name
                || stable == device_id_or_name
                || stable_io == device_id_or_name
            {
                return midi_out.connect(&port, "Futureboard MIDI Out").ok();
            }
            let ordinal = stable_id_ordinal(device_id_or_name, &name);
            if ordinal > 1 {
                let matching = ports.iter().filter(|candidate| {
                    midi_out
                        .port_name(candidate)
                        .ok()
                        .map(|candidate_name| normalize_port_name(&candidate_name))
                        .as_deref()
                        == Some(name.as_str())
                });
                if let Some(target) = matching.into_iter().nth(ordinal - 1) {
                    return midi_out.connect(target, "Futureboard MIDI Out").ok();
                }
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        macos_coremidi::open_output(device_id_or_name)
    }
}

fn send_all_notes_off(connections: &mut HashMap<String, MidiOutputConnection>) {
    for conn in connections.values_mut() {
        for channel in 0..16u8 {
            for note in 0..128u8 {
                let _ = conn.send(&[0x80 | channel, note, 0]);
            }
            let _ = conn.send(&[0xb0 | channel, 64, 0]);
            let _ = conn.send(&[0xb0 | channel, 123, 0]);
            let _ = conn.send(&[0xb0 | channel, 120, 0]);
        }
    }
}

fn seconds_to_samples(seconds: f64, sample_rate: u32) -> u64 {
    (seconds.max(0.0) * sample_rate.max(1) as f64).round() as u64
}

fn samples_to_duration(samples: u64, sample_rate: f64) -> Duration {
    Duration::from_secs_f64(samples as f64 / sample_rate.max(1.0))
}

fn midi_output_debug_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_MIDI_OUTPUT_DEBUG").is_some())
}

fn midi_lateness_warnings_enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_MIDI_OUTPUT_LATENESS_WARN").is_some())
}

fn sort_hardware_midi_events(events: &mut [HardwareMidiEvent]) {
    events.sort_by(|a, b| {
        a.absolute_sample
            .cmp(&b.absolute_sample)
            .then_with(|| midi_order_key(a).cmp(&midi_order_key(b)))
            .then_with(|| a.device_id.cmp(&b.device_id))
            .then_with(|| a.message.cmp(&b.message))
    });
}

fn midi_order_key(event: &HardwareMidiEvent) -> (u8, u8, u8, u8) {
    let status = event.message.first().copied().unwrap_or(0);
    let kind = status & 0xf0;
    let channel = status & 0x0f;
    let data1 = event.message.get(1).copied().unwrap_or(0);
    let data2 = event.message.get(2).copied().unwrap_or(0);
    let group = match kind {
        // SysEx leads its timestamp: a GS/XG/GM reset or a part setup has to
        // land before the program changes and notes it configures, or the
        // reset wipes them.
        0xf0 if status == 0xf0 => 0,
        0x80 => 1,
        0x90 if data2 == 0 => 1,
        0xb0 if data1 == 64 && data2 == 0 => 2,
        0xb0 if data1 == 120 || data1 == 123 => 2,
        0xc0 => 3,
        0xb0 if data1 == 0 || data1 == 32 => 4,
        0xb0 => 5,
        0xe0 => 6,
        0xa0 | 0xd0 => 7,
        0x90 => 8,
        0xf0 => 9,
        _ => 10,
    };
    (group, channel, data1, data2)
}

fn coalesce_hardware_midi_events(
    events: Vec<HardwareMidiEvent>,
    sample_rate: u32,
) -> Vec<HardwareMidiEvent> {
    let close_window = seconds_to_samples(0.002, sample_rate).max(1);
    let dense_window = seconds_to_samples(0.005, sample_rate).max(1);
    let mut out: Vec<HardwareMidiEvent> = Vec::with_capacity(events.len());
    let mut last_cc: HashMap<(String, u8, u8), (u8, u64)> = HashMap::new();
    let mut last_pb: HashMap<(String, u8), (u16, u64)> = HashMap::new();

    for event in events {
        let Some(status) = event.message.first().copied() else {
            out.push(event);
            continue;
        };
        let kind = status & 0xf0;
        let channel = status & 0x0f;
        match kind {
            0xb0 => {
                let controller = event.message.get(1).copied().unwrap_or(0);
                let value = event.message.get(2).copied().unwrap_or(0);
                // Preserve bank select, sustain pedal transitions, and panic CCs.
                if matches!(controller, 0 | 32 | 64 | 120 | 123) {
                    out.push(event);
                    continue;
                }
                let key = (event.device_id.clone(), channel, controller);
                if let Some((prev_value, prev_sample)) = last_cc.get(&key).copied() {
                    let delta = event.absolute_sample.saturating_sub(prev_sample);
                    if prev_value == value && delta <= close_window {
                        continue;
                    }
                    if delta <= dense_window {
                        if let Some(last) = out.iter_mut().rev().find(|candidate| {
                            candidate.device_id == event.device_id
                                && candidate.message.first().copied().unwrap_or(0) & 0xf0 == 0xb0
                                && candidate.message.first().copied().unwrap_or(0) & 0x0f == channel
                                && candidate.message.get(1).copied().unwrap_or(255) == controller
                        }) {
                            *last = event.clone();
                        } else {
                            out.push(event.clone());
                        }
                        last_cc.insert(key, (value, event.absolute_sample));
                        continue;
                    }
                }
                last_cc.insert(key, (value, event.absolute_sample));
                out.push(event);
            }
            0xe0 => {
                let lsb = event.message.get(1).copied().unwrap_or(0) as u16;
                let msb = event.message.get(2).copied().unwrap_or(0) as u16;
                let value = (msb << 7) | lsb;
                let key = (event.device_id.clone(), channel);
                if let Some((prev_value, prev_sample)) = last_pb.get(&key).copied() {
                    let delta = event.absolute_sample.saturating_sub(prev_sample);
                    if prev_value == value && delta <= close_window {
                        continue;
                    }
                    if delta <= dense_window {
                        if let Some(last) = out.iter_mut().rev().find(|candidate| {
                            candidate.device_id == event.device_id
                                && candidate.message.first().copied().unwrap_or(0) & 0xf0 == 0xe0
                                && candidate.message.first().copied().unwrap_or(0) & 0x0f == channel
                        }) {
                            *last = event.clone();
                        } else {
                            out.push(event.clone());
                        }
                        last_pb.insert(key, (value, event.absolute_sample));
                        continue;
                    }
                }
                last_pb.insert(key, (value, event.absolute_sample));
                out.push(event);
            }
            _ => out.push(event),
        }
    }

    sort_hardware_midi_events(&mut out);
    out
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn unique_event_devices(events: &[HardwareMidiEvent]) -> Vec<String> {
    let mut devices = Vec::new();
    for event in events {
        if !devices.iter().any(|device| device == &event.device_id) {
            devices.push(event.device_id.clone());
        }
    }
    devices
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn log_midi_dispatch(
    event: &HardwareMidiEvent,
    scheduled_wall: Instant,
    actual: Instant,
    lateness: Duration,
    debug: bool,
    warnings: bool,
) {
    let lateness_ms = lateness.as_secs_f64() * 1000.0;
    if debug {
        let (kind, ch, d1, d2) = describe_midi_message(&event.message);
        eprintln!(
            "[midi-output] send type={kind} ch={ch} data1={d1} data2={d2} beat={:.6} sample={} scheduled={scheduled_wall:?} actual={actual:?} late_ms={lateness_ms:.3}",
            event.beat, event.absolute_sample,
        );
    }
    if warnings && lateness_ms >= 2.0 {
        let threshold = if lateness_ms >= 20.0 {
            20
        } else if lateness_ms >= 10.0 {
            10
        } else if lateness_ms >= 5.0 {
            5
        } else {
            2
        };
        eprintln!(
            "[midi-output] WARNING lateness>{threshold}ms actual={lateness_ms:.3} sample={} beat={:.6}",
            event.absolute_sample, event.beat
        );
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn describe_midi_message(message: &[u8]) -> (&'static str, u8, u8, u8) {
    let status = message.first().copied().unwrap_or(0);
    let channel = status & 0x0f;
    let d1 = message.get(1).copied().unwrap_or(0);
    let d2 = message.get(2).copied().unwrap_or(0);
    let kind = match status & 0xf0 {
        0x80 => "note_off",
        0x90 if d2 == 0 => "note_off",
        0x90 => "note_on",
        0xa0 => "poly_aftertouch",
        0xb0 => "cc",
        0xc0 => "program_change",
        0xd0 => "channel_aftertouch",
        0xe0 => "pitch_bend",
        0xf0 => "sysex/system",
        _ => "unknown",
    };
    (kind, channel, d1, d2)
}

#[derive(Default)]
struct HardwareMidiProfiler {
    events_total: std::sync::atomic::AtomicU64,
    max_jitter_us: std::sync::atomic::AtomicU32,
    started: OnceLock<Instant>,
}

impl HardwareMidiProfiler {
    fn reset(&self) {
        self.events_total
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.max_jitter_us
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn record(&self, lateness: Duration) {
        let _ = self.started.get_or_init(Instant::now);
        self.events_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let us = lateness.as_micros().min(u32::MAX as u128) as u32;
        let mut prev = self
            .max_jitter_us
            .load(std::sync::atomic::Ordering::Relaxed);
        while us > prev {
            match self.max_jitter_us.compare_exchange_weak(
                prev,
                us,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => prev = next,
            }
        }
    }

    fn snapshot(&self) -> HardwareMidiProfilerSnapshot {
        let elapsed = self
            .started
            .get()
            .map(|started| started.elapsed().as_secs_f64())
            .unwrap_or(0.0)
            .max(0.001);
        HardwareMidiProfilerSnapshot {
            platform: std::env::consts::OS,
            events_per_second: (self.events_total.load(std::sync::atomic::Ordering::Relaxed) as f64
                / elapsed)
                .round()
                .min(u32::MAX as f64) as u32,
            max_jitter_us: self
                .max_jitter_us
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

#[cfg(target_os = "windows")]
struct MidiThreadScope;

#[cfg(target_os = "windows")]
impl MidiThreadScope {
    fn enter() -> Self {
        unsafe {
            let _ = timeBeginPeriod(1);
            set_current_thread_priority_high();
        }
        Self
    }
}

#[cfg(target_os = "windows")]
impl Drop for MidiThreadScope {
    fn drop(&mut self) {
        unsafe {
            let _ = timeEndPeriod(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
struct MidiThreadScope;

#[cfg(not(target_os = "windows"))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
impl MidiThreadScope {
    fn enter() -> Self {
        #[cfg(target_os = "linux")]
        promote_linux_midi_thread();
        Self
    }
}

#[cfg(target_os = "windows")]
fn wait_for_midi_tick(duration: Duration) {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, SetWaitableTimer,
        TIMER_ALL_ACCESS, WaitForSingleObject,
    };

    struct HighResolutionTimer(windows::Win32::Foundation::HANDLE);

    impl Drop for HighResolutionTimer {
        fn drop(&mut self) {
            // SAFETY: this wrapper uniquely owns the handle.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    thread_local! {
        // One timer per MIDI scheduler thread. Reusing it avoids a kernel
        // handle create/close pair on every 1 ms scheduling slice.
        static TIMER: Option<HighResolutionTimer> = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
            .ok()
            .map(HighResolutionTimer)
        };
    }

    let due_100ns = -(duration.as_nanos().min(i64::MAX as u128 / 100) as i64 / 100).max(-1);
    let waited = TIMER.with(|timer| {
        let Some(timer) = timer else {
            return false;
        };
        let mut due = due_100ns;
        // SAFETY: the timer remains alive for the entire TLS closure and the
        // due-time pointer is valid for the call.
        unsafe {
            SetWaitableTimer(timer.0, &mut due, 0, None, None, false).is_ok()
                && WaitForSingleObject(timer.0, u32::MAX) == WAIT_OBJECT_0
        }
    });
    if !waited {
        // Rare old-Windows / resource-exhaustion fallback. `park_timeout`
        // avoids returning to `thread::sleep` while retaining cancellation
        // checks at the caller's bounded 1 ms cadence.
        std::thread::park_timeout(duration);
    }
}

#[cfg(unix)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn wait_for_midi_tick(duration: Duration) {
    // POSIX nanosleep is backed by the platform's high-resolution monotonic
    // timer and preserves the remaining interval across signal interruption.
    // The caller recomputes the musical deadline after every bounded wait, so
    // it cannot accumulate drift.
    let mut request = libc::timespec {
        tv_sec: duration.as_secs().min(libc::time_t::MAX as u64) as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    };
    loop {
        let mut remaining = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: both pointers reference initialized timespec values.
        if unsafe { libc::nanosleep(&request, &mut remaining) } == 0 {
            break;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
        request = remaining;
    }
}

#[cfg(not(any(target_os = "windows", unix)))]
fn wait_for_midi_tick(duration: Duration) {
    std::thread::park_timeout(duration);
}

#[cfg(target_os = "linux")]
fn promote_linux_midi_thread() {
    const FIFO_PRIORITIES: [libc::c_int; 3] = [60, 20, 5];
    const FALLBACK_NICE: libc::c_int = -8;
    for priority in FIFO_PRIORITIES {
        // SAFETY: the structure is fully initialized and pid 0 is the calling
        // thread on Linux.
        let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
        param.sched_priority = priority;
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } == 0 {
            eprintln!("[midi-output] scheduler=SCHED_FIFO priority={priority}");
            return;
        }
    }
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, FALLBACK_NICE) } == 0 {
        eprintln!(
            "[midi-output] realtime permission denied; scheduler=SCHED_OTHER nice={FALLBACK_NICE}"
        );
    } else {
        eprintln!(
            "[midi-output] realtime permission denied and nice fallback unavailable: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(target_os = "windows")]
unsafe fn set_current_thread_priority_high() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

#[cfg(target_os = "windows")]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(uperiod: u32) -> u32;
    fn timeEndPeriod(uperiod: u32) -> u32;
}

pub fn migrate_legacy_midi_settings(midi: &mut MidiHardwareSettings) {
    if !midi.devices.is_empty() {
        midi.enabled_inputs.clear();
        midi.enabled_outputs.clear();
        return;
    }

    let mut devices = Vec::new();
    for name in &midi.enabled_inputs {
        devices.push(MidiDeviceSetting {
            id: stable_id(MidiDeviceDirection::Input, name),
            name: name.clone(),
            direction: MidiDeviceDirection::Input,
            enabled: true,
            connected: true,
            clock_enabled: false,
        });
    }
    for name in &midi.enabled_outputs {
        devices.push(MidiDeviceSetting {
            id: stable_id(MidiDeviceDirection::Output, name),
            name: name.clone(),
            direction: MidiDeviceDirection::Output,
            enabled: true,
            connected: true,
            clock_enabled: midi.clock_sync,
        });
    }
    if !devices.is_empty() {
        midi.devices = devices;
    }
    midi.enabled_inputs.clear();
    midi.enabled_outputs.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Cross-platform port naming ──────────────────────────────────────────

    /// Linux/ALSA appends a volatile `client:port` address that changes on
    /// every replug. Stripping it is what keeps a device's identity — and so
    /// its saved enabled state — stable across sessions.
    #[test]
    fn an_alsa_sequencer_address_is_stripped_from_the_port_name() {
        assert_eq!(
            normalize_port_name("Launchkey Mini:Launchkey Mini MIDI 1 24:0"),
            "Launchkey Mini:Launchkey Mini MIDI 1"
        );
        assert_eq!(
            normalize_port_name("Midi Through:Midi Through Port-0 14:0"),
            "Midi Through:Midi Through Port-0"
        );
    }

    #[test]
    fn a_replugged_alsa_port_keeps_its_stable_id() {
        let before = normalize_port_name("Launchkey Mini:Launchkey Mini MIDI 1 24:0");
        let after = normalize_port_name("Launchkey Mini:Launchkey Mini MIDI 1 28:0");
        assert_eq!(before, after);
        assert_eq!(
            stable_id(MidiDeviceDirection::Input, &before),
            stable_id(MidiDeviceDirection::Input, &after),
            "a replug must not mint a new device identity"
        );
    }

    /// Windows and macOS names must survive untouched — including the colons
    /// CoreMIDI and WinMM names legitimately contain.
    #[test]
    fn windows_and_macos_names_are_left_alone() {
        for name in [
            "Launchkey Mini MK3",
            "IAC Driver Bus 1",
            "MPKmini2",
            "Scarlett 18i20 USB",
            // A colon that is not an ALSA address.
            "Roland: SC-88",
            "Port 1:2 Extra",
        ] {
            assert_eq!(normalize_port_name(name), name, "{name} must not change");
        }
    }

    #[test]
    fn a_trailing_non_numeric_suffix_is_not_mistaken_for_an_address() {
        assert_eq!(normalize_port_name("Device MIDI a:0"), "Device MIDI a:0");
        assert_eq!(normalize_port_name("Device MIDI 24:b"), "Device MIDI 24:b");
        assert_eq!(normalize_port_name("Device MIDI 24:"), "Device MIDI 24:");
        assert_eq!(normalize_port_name("Device MIDI :0"), "Device MIDI :0");
    }

    /// Normalizing must never leave a port unnameable.
    #[test]
    fn an_address_only_name_is_preserved() {
        assert_eq!(normalize_port_name("24:0"), "24:0");
        assert_eq!(normalize_port_name(" 24:0"), " 24:0");
        assert_eq!(normalize_port_name(""), "");
    }

    #[test]
    fn normalizing_is_idempotent() {
        let once = normalize_port_name("Launchkey Mini:Launchkey Mini MIDI 1 24:0");
        assert_eq!(normalize_port_name(&once), once);
    }

    /// The saved-vs-detected merge keys on id and name, so a normalized name
    /// carries the user's enabled state across a replug.
    #[test]
    fn a_replugged_linux_device_keeps_its_saved_enabled_state() {
        let name = normalize_port_name("Launchkey Mini:Launchkey Mini MIDI 1 24:0");
        let saved = vec![MidiDeviceSetting {
            id: stable_id(MidiDeviceDirection::Input, &name),
            name: name.clone(),
            direction: MidiDeviceDirection::Input,
            enabled: false,
            connected: true,
            clock_enabled: false,
        }];
        // Same hardware, new sequencer address.
        let replugged = normalize_port_name("Launchkey Mini:Launchkey Mini MIDI 1 31:0");
        let detected = vec![DetectedMidiDevice {
            id: stable_id(MidiDeviceDirection::Input, &replugged),
            name: replugged,
            direction: MidiDeviceDirection::Input,
        }];

        let resolved = resolve_midi_devices(&saved, &detected);
        assert_eq!(resolved.len(), 1, "no phantom duplicate row");
        assert!(resolved[0].connected);
        assert!(
            !resolved[0].enabled,
            "the user's disabled choice must survive the replug"
        );
    }

    #[test]
    fn resolve_keeps_missing_saved_devices() {
        let saved = vec![MidiDeviceSetting {
            id: "midi-in-old".to_string(),
            name: "Old Controller".to_string(),
            direction: MidiDeviceDirection::Input,
            enabled: true,
            connected: true,
            clock_enabled: false,
        }];
        let detected = vec![DetectedMidiDevice {
            id: "midi-in-new".to_string(),
            name: "New Controller".to_string(),
            direction: MidiDeviceDirection::Input,
        }];
        let resolved = resolve_midi_devices(&saved, &detected);
        assert_eq!(resolved.len(), 2);
        assert!(
            resolved
                .iter()
                .any(|d| d.id == "midi-in-old" && !d.connected)
        );
        assert!(
            resolved
                .iter()
                .any(|d| d.id == "midi-in-new" && d.connected && d.enabled)
        );
    }

    #[test]
    fn coalesce_merges_same_name_input_and_output() {
        let detected = vec![
            DetectedMidiDevice {
                id: "midi-in-pad".to_string(),
                name: "Pad Controller".to_string(),
                direction: MidiDeviceDirection::Input,
            },
            DetectedMidiDevice {
                id: "midi-out-pad".to_string(),
                name: "Pad Controller".to_string(),
                direction: MidiDeviceDirection::Output,
            },
            DetectedMidiDevice {
                id: "midi-in-keys".to_string(),
                name: "Keys".to_string(),
                direction: MidiDeviceDirection::Input,
            },
        ];
        let coalesced = coalesce_detected_midi_devices(detected);
        assert_eq!(coalesced.len(), 2);
        let pad = coalesced
            .iter()
            .find(|d| d.name == "Pad Controller")
            .expect("pad");
        assert_eq!(pad.direction, MidiDeviceDirection::InputOutput);
        assert!(pad.id.starts_with("midi-io-"));
        let keys = coalesced.iter().find(|d| d.name == "Keys").expect("keys");
        assert_eq!(keys.direction, MidiDeviceDirection::Input);
    }

    #[test]
    fn coalesce_preserves_duplicate_named_physical_ports() {
        let detected = vec![
            DetectedMidiDevice {
                id: String::new(),
                name: "USB MIDI".to_string(),
                direction: MidiDeviceDirection::Input,
            },
            DetectedMidiDevice {
                id: String::new(),
                name: "USB MIDI".to_string(),
                direction: MidiDeviceDirection::Input,
            },
            DetectedMidiDevice {
                id: String::new(),
                name: "USB MIDI".to_string(),
                direction: MidiDeviceDirection::Output,
            },
            DetectedMidiDevice {
                id: String::new(),
                name: "USB MIDI".to_string(),
                direction: MidiDeviceDirection::Output,
            },
        ];
        let coalesced = coalesce_detected_midi_devices(detected);
        assert_eq!(coalesced.len(), 2);
        assert!(
            coalesced
                .iter()
                .all(|device| device.direction == MidiDeviceDirection::InputOutput)
        );
        assert_eq!(coalesced[0].id, "midi-io-usb-midi");
        assert_eq!(coalesced[1].id, "midi-io-usb-midi-2");
        assert_eq!(stable_id_ordinal(&coalesced[0].id, "USB MIDI"), 1);
        assert_eq!(stable_id_ordinal(&coalesced[1].id, "USB MIDI"), 2);
    }

    #[test]
    fn resolve_dedupes_saved_clone_when_live_name_matches() {
        let saved = vec![MidiDeviceSetting {
            id: "midi-in-pad".to_string(),
            name: "Pad Controller".to_string(),
            direction: MidiDeviceDirection::Input,
            enabled: true,
            connected: false,
            clock_enabled: false,
        }];
        let detected = vec![DetectedMidiDevice {
            id: "midi-io-pad-controller".to_string(),
            name: "Pad Controller".to_string(),
            direction: MidiDeviceDirection::InputOutput,
        }];
        let resolved = resolve_midi_devices(&saved, &detected);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "midi-io-pad-controller");
        assert!(resolved[0].enabled);
        assert!(resolved[0].connected);
    }

    #[test]
    fn migrate_legacy_inputs_outputs() {
        let mut midi = MidiHardwareSettings {
            devices: Vec::new(),
            clock_sync: true,
            enabled_inputs: vec!["Keyboard Controller".to_string()],
            enabled_outputs: vec!["Interface".to_string()],
        };
        migrate_legacy_midi_settings(&mut midi);
        assert_eq!(midi.devices.len(), 2);
        assert!(midi.enabled_inputs.is_empty());
        assert!(midi.enabled_outputs.is_empty());
    }

    fn hw_event(sample: u64, message: &[u8]) -> HardwareMidiEvent {
        HardwareMidiEvent {
            device_id: "midi-out-test".to_string(),
            delay_seconds: sample as f64 / 48_000.0,
            beat: sample as f64 / 24_000.0,
            absolute_sample: sample,
            message: message.to_vec(),
        }
    }

    #[test]
    fn hardware_sort_sends_note_off_before_note_on_at_same_sample() {
        let mut events = vec![
            hw_event(100, &[0x90, 60, 100]),
            hw_event(100, &[0xb0, 1, 64]),
            hw_event(100, &[0x80, 60, 0]),
        ];
        sort_hardware_midi_events(&mut events);
        assert_eq!(events[0].message, vec![0x80, 60, 0]);
        assert_eq!(events[1].message, vec![0xb0, 1, 64]);
        assert_eq!(events[2].message, vec![0x90, 60, 100]);
    }

    #[test]
    fn hardware_coalescing_drops_dense_cc_and_pitch_bend_but_keeps_notes() {
        let events = vec![
            hw_event(100, &[0x90, 60, 100]),
            hw_event(101, &[0xb0, 1, 10]),
            hw_event(102, &[0xb0, 1, 10]),
            hw_event(103, &[0xb0, 1, 20]),
            hw_event(104, &[0xe0, 0, 64]),
            hw_event(105, &[0xe0, 0, 64]),
            hw_event(106, &[0x80, 60, 0]),
        ];
        let coalesced = coalesce_hardware_midi_events(events, 48_000);
        assert!(
            coalesced
                .iter()
                .any(|event| event.message == vec![0x90, 60, 100])
        );
        assert!(
            coalesced
                .iter()
                .any(|event| event.message == vec![0x80, 60, 0])
        );
        assert_eq!(
            coalesced
                .iter()
                .filter(|event| event.message.first().copied().unwrap_or(0) & 0xf0 == 0xb0)
                .count(),
            1
        );
        assert_eq!(
            coalesced
                .iter()
                .filter(|event| event.message.first().copied().unwrap_or(0) & 0xf0 == 0xe0)
                .count(),
            1
        );
    }

    /// Split `bytes` with a fresh running status and collect the messages.
    fn split(bytes: &[u8]) -> (Vec<Vec<u8>>, usize) {
        let mut messages = Vec::new();
        let mut running = None;
        let malformed = split_midi_messages(bytes, &mut running, |message| {
            messages.push(message.to_vec());
        });
        (messages, malformed)
    }

    #[test]
    fn a_single_message_packet_yields_one_message() {
        let (messages, malformed) = split(&[0x90, 60, 100]);
        assert_eq!(messages, vec![vec![0x90, 60, 100]]);
        assert_eq!(malformed, 0);
    }

    #[test]
    fn a_coalesced_packet_yields_every_message() {
        // CoreMIDI packs messages that share a timestamp into one packet, so a
        // chord arrives as a single payload. Decoding only the first three
        // bytes would play one note out of three.
        let (messages, malformed) = split(&[0x90, 60, 100, 0x90, 64, 100, 0x90, 67, 100]);
        assert_eq!(
            messages,
            vec![
                vec![0x90, 60, 100],
                vec![0x90, 64, 100],
                vec![0x90, 67, 100]
            ]
        );
        assert_eq!(malformed, 0);

        let events: Vec<_> = messages
            .iter()
            .filter_map(|message| decode_midi_bytes(message))
            .collect();
        assert_eq!(events.len(), 3, "every note in the chord must decode");
    }

    #[test]
    fn running_status_reuses_the_previous_status_byte() {
        let (messages, malformed) = split(&[0x90, 60, 100, 64, 100, 67, 0]);
        assert_eq!(
            messages,
            vec![vec![0x90, 60, 100], vec![0x90, 64, 100], vec![0x90, 67, 0]]
        );
        assert_eq!(malformed, 0);
        // Note On with velocity 0 is the usual Note Off from a keyboard.
        assert_eq!(
            decode_midi_bytes(&messages[2]),
            Some(MidiInputEvent::NoteOff {
                note: 67,
                channel: 0
            })
        );
    }

    #[test]
    fn realtime_bytes_do_not_break_the_surrounding_message() {
        // Clock and active sensing are legal between the data bytes of another
        // message. They must come out standalone and leave running status alone.
        let (messages, malformed) = split(&[0xF8, 0x90, 60, 100, 0xFE, 64, 100]);
        assert_eq!(
            messages,
            vec![
                vec![0xF8],
                vec![0x90, 60, 100],
                vec![0xFE],
                vec![0x90, 64, 100],
            ]
        );
        assert_eq!(malformed, 0);
        assert_eq!(decode_midi_bytes(&[0xF8]), None, "clock is not routed");
    }

    #[test]
    fn system_common_clears_running_status() {
        let (messages, malformed) = split(&[0x90, 60, 100, 0xF2, 0, 1, 64, 100]);
        assert_eq!(
            messages,
            vec![vec![0x90, 60, 100], vec![0xF2, 0, 1]],
            "song position must not be followed by a resurrected Note On"
        );
        // The trailing 64, 100 have no status to attach to once F2 cleared it.
        assert_eq!(malformed, 2);
    }

    #[test]
    fn channel_is_preserved_across_the_split() {
        let (messages, _) = split(&[0x93, 60, 100, 0x8A, 60, 0]);
        assert_eq!(
            decode_midi_bytes(&messages[0]),
            Some(MidiInputEvent::NoteOn {
                note: 60,
                velocity: 100,
                channel: 3
            })
        );
        assert_eq!(
            decode_midi_bytes(&messages[1]),
            Some(MidiInputEvent::NoteOff {
                note: 60,
                channel: 10
            })
        );
    }

    #[test]
    fn control_change_and_two_byte_messages_frame_correctly() {
        // Program change / channel pressure carry one data byte; getting the
        // count wrong would shift every following message.
        let (messages, malformed) = split(&[0xC0, 5, 0xD0, 64, 0xB0, 7, 100]);
        assert_eq!(
            messages,
            vec![vec![0xC0, 5], vec![0xD0, 64], vec![0xB0, 7, 100]]
        );
        assert_eq!(malformed, 0);
        assert_eq!(
            decode_midi_bytes(&messages[2]),
            Some(MidiInputEvent::ControlChange {
                controller: 7,
                value: 100,
                channel: 0
            })
        );
    }

    #[test]
    fn sysex_is_consumed_whole_and_does_not_swallow_the_next_message() {
        let (messages, malformed) = split(&[0xF0, 0x7E, 0x00, 0x06, 0xF7, 0x90, 60, 100]);
        assert_eq!(
            messages,
            vec![vec![0xF0, 0x7E, 0x00, 0x06, 0xF7], vec![0x90, 60, 100]]
        );
        assert_eq!(malformed, 0);
        assert_eq!(decode_midi_bytes(&messages[0]), None);
    }

    #[test]
    fn a_truncated_message_is_reported_not_guessed() {
        let (messages, malformed) = split(&[0x90, 60]);
        assert!(messages.is_empty());
        assert_eq!(malformed, 2);
    }

    #[test]
    fn a_packet_starting_mid_message_does_not_invent_a_status() {
        let (messages, malformed) = split(&[60, 100]);
        assert!(messages.is_empty());
        assert_eq!(malformed, 2);
    }

    #[test]
    fn running_status_carries_across_packet_boundaries() {
        // The caller threads one running-status slot through every packet of a
        // list, because a packet boundary is not a message boundary.
        let mut running = None;
        let mut messages = Vec::new();
        split_midi_messages(&[0x90, 60, 100], &mut running, |m| {
            messages.push(m.to_vec())
        });
        split_midi_messages(&[64, 100], &mut running, |m| messages.push(m.to_vec()));
        assert_eq!(messages, vec![vec![0x90, 60, 100], vec![0x90, 64, 100]]);
    }

    #[test]
    fn an_empty_payload_is_a_no_op() {
        let (messages, malformed) = split(&[]);
        assert!(messages.is_empty());
        assert_eq!(malformed, 0);
    }

    #[test]
    fn diagnostics_counters_are_monotonic() {
        let before = midi_input_diagnostics();
        note_midi_input_callback(3, 2, 1, 0, 0);
        note_midi_input_routed(true);
        note_midi_input_routed(false);
        let after = midi_input_diagnostics();
        // Process-wide statics with tests running in parallel: assert the
        // deltas cover at least this call, not exact totals.
        assert!(after.callbacks > before.callbacks);
        assert!(after.messages - before.messages >= 3);
        assert!(after.events - before.events >= 2);
        assert!(after.ignored - before.ignored >= 1);
        assert!(after.routed > before.routed);
        assert!(after.unrouted > before.unrouted);
    }
}
