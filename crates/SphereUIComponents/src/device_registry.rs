//! Process-wide Audio + MIDI device registry.
//!
//! Replaces the old mock device lists. Real scans run once at Splash/Welcome
//! startup (and on manual Refresh), and Preferences renders from the cached
//! snapshot — it never re-scans on every paint. The registry is a plain
//! `static` rather than a `gpui::Global` because the Welcome window opens before
//! the studio globals exist and both windows need to reach the same state (see
//! the `welcome-settings-no-global` note).
//!
//! Scans are cheap and non-destructive: MIDI port enumeration (`midir`) and
//! cpal device enumeration do not open the hardware. Both are wrapped so a
//! backend failure yields an empty list + warning instead of a panic.

use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use sphere_midi_service::{scan_midi_ports, DetectedMidiDevice};

/// One enumerated audio endpoint. Mirrors the engine's `JsAudioDeviceInfo`
/// fields we actually surface in Preferences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDeviceEntry {
    pub id: String,
    pub name: String,
    pub channels: u32,
    pub default_sample_rate: u32,
    pub is_default: bool,
}

/// Immutable snapshot of the audio side of the registry.
#[derive(Debug, Clone, Default)]
pub struct AudioDeviceSnapshot {
    pub inputs: Vec<AudioDeviceEntry>,
    pub outputs: Vec<AudioDeviceEntry>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
    pub backend: String,
    pub revision: u64,
}

#[derive(Default)]
struct RegistryState {
    midi: Vec<DetectedMidiDevice>,
    midi_revision: u64,
    midi_scanned: bool,
    midi_consecutive_empty_scans: u8,
    audio: AudioDeviceSnapshot,
    audio_scanned: bool,
    /// See [`set_selected_audio_devices`].
    selected_audio_devices: (String, String),
}

fn state() -> &'static RwLock<RegistryState> {
    static STATE: OnceLock<RwLock<RegistryState>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(RegistryState::default()))
}

// ── MIDI ────────────────────────────────────────────────────────────────────

/// Run a real MIDI port scan, replace the cache, bump the revision, and return
/// the new revision. Called at startup and on manual Refresh — never per paint.
fn commit_midi_scan(
    devices: Vec<DetectedMidiDevice>,
    confirmed_empty: bool,
    start: Instant,
) -> u64 {
    let reported_devices = devices.len();
    let (revision, cached_devices, preserved_previous) = {
        let mut s = state().write().unwrap();
        let empty = devices.is_empty();
        if empty {
            s.midi_consecutive_empty_scans = s.midi_consecutive_empty_scans.saturating_add(1);
        } else {
            s.midi_consecutive_empty_scans = 0;
        }
        let preserve_previous = should_preserve_midi_cache(
            !s.midi.is_empty(),
            empty,
            confirmed_empty,
            s.midi_consecutive_empty_scans,
        );
        if !preserve_previous {
            s.midi = devices;
        }
        s.midi_revision = s.midi_revision.wrapping_add(1);
        s.midi_scanned = true;
        (s.midi_revision, s.midi.len(), preserve_previous)
    };
    if sphere_midi_service::midi_settings_debug_enabled() || reported_devices == 0 {
        eprintln!(
            "[MidiDeviceScan] reported={reported_devices} cached={cached_devices} preserved_previous={preserved_previous} duration={:.1}ms revision={revision}",
            start.elapsed().as_secs_f32() * 1000.0
        );
    }
    revision
}

fn should_preserve_midi_cache(
    had_devices: bool,
    scan_empty: bool,
    confirmed_empty: bool,
    consecutive_empty_scans: u8,
) -> bool {
    had_devices && scan_empty && !confirmed_empty && consecutive_empty_scans < 2
}

fn resilient_midi_scan_with(
    mut scan: impl FnMut() -> Vec<DetectedMidiDevice>,
    mut wait: impl FnMut(Duration),
) -> Vec<DetectedMidiDevice> {
    const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(40), Duration::from_millis(120)];
    let mut devices = scan();
    for delay in RETRY_DELAYS {
        if !devices.is_empty() {
            break;
        }
        wait(delay);
        devices = scan();
    }
    devices
}

/// Single non-blocking enumeration attempt. A lone transient empty result does
/// not erase a previously healthy cache; two consecutive manual scans confirm
/// that all devices were actually disconnected.
pub fn scan_midi() -> u64 {
    let start = Instant::now();
    let devices = scan_midi_ports();
    commit_midi_scan(devices, false, start)
}

/// Startup/manual scan for backends that initialize asynchronously (WinMM,
/// ALSA sequencer, CoreMIDI). Must be called off the UI/audio thread.
pub fn scan_midi_resilient() -> u64 {
    let start = Instant::now();
    let devices = resilient_midi_scan_with(scan_midi_ports, std::thread::sleep);
    let confirmed_empty = devices.is_empty();
    commit_midi_scan(devices, confirmed_empty, start)
}

/// Cached MIDI devices for rendering. Lazily runs one scan the first time it is
/// read if startup never scanned (e.g. the `--skip-splash` path), so the cache
/// is correct without ever scanning on a hot render path.
pub fn cached_midi_devices() -> Vec<DetectedMidiDevice> {
    // A platform hot-plug notification (macOS CoreMIDI today) invalidates the
    // cache exactly once, so plugging a controller in shows up without the user
    // hunting for a Rescan button. Platforms with no notification answer false
    // and keep the previous explicit-rescan behaviour.
    if !state().read().unwrap().midi_scanned || sphere_midi_service::take_midi_ports_changed() {
        scan_midi();
    }
    state().read().unwrap().midi.clone()
}

pub fn midi_revision() -> u64 {
    state().read().unwrap().midi_revision
}

// ── Audio ───────────────────────────────────────────────────────────────────

fn to_entry(info: DirectAudio::types::JsAudioDeviceInfo) -> AudioDeviceEntry {
    AudioDeviceEntry {
        id: info.id,
        name: info.name,
        channels: info.channels,
        default_sample_rate: info.default_sample_rate,
        is_default: info.is_default,
    }
}

fn engine_entry(info: DirectAudio::EngineDeviceInfo) -> AudioDeviceEntry {
    AudioDeviceEntry {
        id: info.id,
        name: info.name,
        channels: info.channels,
        default_sample_rate: info.default_sample_rate,
        is_default: info.is_default,
    }
}

fn commit_audio_scan(
    inputs: Vec<AudioDeviceEntry>,
    outputs: Vec<AudioDeviceEntry>,
    backend: String,
    start: Instant,
) -> u64 {
    let default_input = inputs.iter().find(|d| d.is_default).map(|d| d.name.clone());
    let default_output = outputs
        .iter()
        .find(|d| d.is_default)
        .map(|d| d.name.clone());
    let (in_n, out_n) = (inputs.len(), outputs.len());
    let revision = {
        let mut s = state().write().unwrap();
        s.audio.revision = s.audio.revision.wrapping_add(1);
        s.audio = AudioDeviceSnapshot {
            inputs,
            outputs,
            default_input,
            default_output,
            backend: backend.clone(),
            revision: s.audio.revision,
        };
        s.audio_scanned = true;
        s.audio.revision
    };
    eprintln!(
        "[AudioDeviceScan] started backend={backend}; found inputs={in_n} outputs={out_n} duration={:.1}ms revision={revision}",
        start.elapsed().as_secs_f32() * 1000.0
    );
    revision
}

/// Run a real audio device scan via the default cpal host, replace the cache,
/// bump the revision, and return the new revision.
pub fn scan_audio() -> u64 {
    use DirectAudio::device::{list_input_devices, list_output_devices};
    let start = Instant::now();
    let raw_in = list_input_devices();
    let raw_out = list_output_devices();
    let backend = raw_out
        .first()
        .or_else(|| raw_in.first())
        .map(|d| d.backend.clone())
        .unwrap_or_default();
    let inputs = raw_in.into_iter().map(to_entry).collect();
    let outputs = raw_out.into_iter().map(to_entry).collect();
    commit_audio_scan(inputs, outputs, backend, start)
}

/// Refresh the shared inventory from the engine's active backend. This is the
/// authoritative path after opening or switching a device: unlike the generic
/// cpal scan it includes backend-specific endpoints such as licensed ASIO
/// drivers and their channel counts.
pub fn scan_audio_for_engine(engine: &DirectAudio::AudioEngine) -> u64 {
    let start = Instant::now();
    let inputs = engine.list_input_devices();
    let outputs = engine.list_output_devices();
    let backend = outputs
        .first()
        .or_else(|| inputs.first())
        .map(|device| device.backend.clone())
        .unwrap_or_else(|| engine.config().backend.display_name().to_string());
    commit_audio_scan(
        inputs.into_iter().map(engine_entry).collect(),
        outputs.into_iter().map(engine_entry).collect(),
        backend,
        start,
    )
}

/// Cached audio snapshot. Lazily scans once if startup never did.
pub fn audio_snapshot() -> AudioDeviceSnapshot {
    if !state().read().unwrap().audio_scanned {
        // Do not let a lazy read from project/session migration enter the
        // CoreAudio HAL. On macOS the supported-config query can block while a
        // remote desktop or another DAW owns the audio session. Settings can
        // still request an explicit refresh once the user is looking at the
        // device list; the empty cache is safe for routing migration because
        // missing hardware is intentionally non-destructive.
        #[cfg(not(target_os = "macos"))]
        scan_audio();
        #[cfg(target_os = "macos")]
        {
            state().write().unwrap().audio_scanned = true;
        }
    }
    state().read().unwrap().audio.clone()
}

/// Publish the input and output devices selected in Audio Device Setup (as
/// stored in settings). Audio Connections runs a bus whose own device is
/// missing on these. Returns whether the selection changed.
pub fn set_selected_audio_devices(input: &str, output: &str) -> bool {
    let mut s = state().write().unwrap();
    let selected = (input.trim().to_string(), output.trim().to_string());
    if s.selected_audio_devices == selected {
        return false;
    }
    s.selected_audio_devices = selected;
    true
}

/// The `(input, output)` last published by [`set_selected_audio_devices`];
/// empty strings before that.
pub fn selected_audio_devices() -> (String, String) {
    state().read().unwrap().selected_audio_devices.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_midi_bumps_revision_and_marks_scanned() {
        // Real enumeration returns whatever the test host has (often empty in
        // CI) — the contract under test is "each scan bumps the revision and
        // marks the cache scanned". The registry is a process-wide static and
        // tests run in parallel, so assert monotonic change, not exact deltas.
        let r1 = scan_midi();
        let r2 = scan_midi();
        assert_ne!(r1, r2, "a second scan must change the revision");
        assert!(midi_revision() >= r2, "revision is monotonic");
        // Cache is populated/marked scanned, so a read never triggers a re-scan.
        let _ = cached_midi_devices();
    }

    #[test]
    fn cached_midi_never_panics() {
        // Lazy scan path must be panic-free even with no MIDI backend.
        let _ = cached_midi_devices();
    }

    #[test]
    fn transient_empty_scan_preserves_previous_devices_once() {
        assert!(should_preserve_midi_cache(true, true, false, 1));
        assert!(!should_preserve_midi_cache(true, true, false, 2));
        assert!(!should_preserve_midi_cache(true, true, true, 1));
        assert!(!should_preserve_midi_cache(false, true, false, 1));
    }

    #[test]
    fn resilient_scan_retries_until_backend_reports_devices() {
        let expected = DetectedMidiDevice {
            id: "midi-in-ready".to_string(),
            name: "Ready Controller".to_string(),
            direction: sphere_midi_service::MidiDeviceDirection::Input,
        };
        let mut attempts = 0;
        let mut waits = Vec::new();
        let devices = resilient_midi_scan_with(
            || {
                attempts += 1;
                if attempts < 3 {
                    Vec::new()
                } else {
                    vec![expected.clone()]
                }
            },
            |delay| waits.push(delay),
        );
        assert_eq!(attempts, 3);
        assert_eq!(
            waits,
            vec![Duration::from_millis(40), Duration::from_millis(120)]
        );
        assert_eq!(devices, vec![expected]);
    }
}
