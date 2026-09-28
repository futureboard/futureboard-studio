//! The engine, and the room as a small mix.
//!
//! Studio routes a jam through a project. This application has no project, so
//! this module is the smallest thing that stands in for one: an engine opened on
//! a device, a list of the people being listened to, and each person's level and
//! mute. Publishing it is one call — the same call Studio makes — because the
//! engine's idea of "who is audible" is a project snapshot either way.
//!
//! Nothing here persists. A room is not a document.

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use DirectAudio::engine::{SharedState, f32_load};
use DirectAudio::types::{
    EngineProjectSnapshot, EngineRoutingSnapshot, EngineTrackInputSourceSnapshot,
    EngineTrackSnapshot,
};
use DirectAudio::{AudioEngine, EngineConfig};

/// One performer this machine is listening to.
#[derive(Clone, Debug, PartialEq)]
pub struct Listener {
    /// The jam device id (`jam:<stream>`), which is the routing identity.
    pub device_id: String,
    /// Their stream's name, as the room announced it.
    pub name: String,
    /// Channels of theirs to take.
    pub channels: Vec<u32>,
    /// Linear gain, `0.0..=2.0`. Unity is 1.0.
    pub volume: f32,
    pub muted: bool,
}

/// What the capture side is doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputStatus {
    /// A capture stream is open and its ring is live.
    pub capture_open: bool,
    /// The engine believes some track wants the input mixed to the output.
    pub monitoring: bool,
    /// Frames the input callback has delivered since the stream opened. Zero
    /// while a stream is nominally open is a device that is not producing —
    /// a muted interface, a wrong channel, a driver that accepted the open and
    /// then said nothing.
    pub frames_captured: u64,
    /// The engine's own last error, which is where a failed device open lands.
    pub last_error: Option<String>,
}

/// What the meters read this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Levels {
    /// Peak of the hardware input pair, before any track touches it — what the
    /// room hears when the live input is being sent.
    pub input: f32,
    /// Peak of the master mix leaving for the speakers.
    pub output: f32,
}

/// The engine plus the room's mix.
pub struct JamMonitor {
    /// The canonical handle. `reopen_with_config` needs `&mut`, so every call
    /// goes through the lock; all of them are control-thread work and none is
    /// on a hot path.
    engine: Mutex<AudioEngine>,
    /// The engine's shared state, for meters. Reading an atomic per frame is
    /// what this is for; it is the same handle the jam bus lives on.
    shared: Arc<SharedState>,
    listening: Mutex<Vec<Listener>>,
    /// Whether this machine hears its own input locally.
    ///
    /// Separate from sending it: a performer wants to hear themselves through
    /// the same buffer everyone else hears them through, and they want that
    /// whether or not the room is listening. It is also the honest way to check
    /// an interface before joining anything.
    self_monitor: AtomicBool,
}

impl JamMonitor {
    /// Open the engine on `config` and publish the empty room.
    pub fn start(config: EngineConfig) -> Result<Self, String> {
        let mut engine = AudioEngine::new(config).map_err(|error| error.to_string())?;
        engine.start().map_err(|error| error.to_string())?;
        let shared = engine.jam_bus();
        let monitor = Self {
            engine: Mutex::new(engine),
            shared,
            listening: Mutex::new(Vec::new()),
            self_monitor: AtomicBool::new(false),
        };
        // The engine needs a project before it has a graph to render into, and
        // an empty one is a valid answer: it is what "in a room, listening to
        // nobody yet" is.
        monitor.publish()?;
        monitor.claim_output()?;
        Ok(monitor)
    }

    /// A cheap clone of the engine handle, for callers that only read.
    pub fn engine(&self) -> Result<AudioEngine, String> {
        Ok(self.locked()?.clone())
    }

    /// Tell the engine that the master mix owns the physical output pair.
    ///
    /// **This is not optional.** The Control Room decides who writes the device
    /// channels, and its answer for "nobody" is to zero the buffer — silence,
    /// never a fallback pair, which is the right rule for a DAW where an
    /// unrouted master must not leak out of an arbitrary output. A host that
    /// never publishes an owner therefore renders a correct mix, meters it, and
    /// then wipes it on the way out, which is exactly the silence this
    /// application had.
    ///
    /// Studio publishes ownership from its Audio Connections routing compile.
    /// There is no routing model here and no Control Room to run, so the answer
    /// is the simple one and it is fixed: Master writes device channels 0/1
    /// directly.
    pub fn claim_output(&self) -> Result<(), String> {
        self.locked()?
            .set_hardware_output_ownership(
                DirectAudio::monitor::HardwareOutputOwner::MasterDirect,
                Some((0, 1)),
                None,
            )
            .map_err(|error| error.to_string())
    }

    /// The device configuration the engine is currently open on.
    pub fn config(&self) -> Result<EngineConfig, String> {
        Ok(self.locked()?.config().clone())
    }

    /// Re-open the device.
    ///
    /// The room survives it: the jam session, its streams and this mix are all
    /// independent of which device the engine happens to be open on, so
    /// changing an interface mid-jam costs the audio a gap and nothing else.
    /// The project is republished afterwards because a re-open builds a fresh
    /// graph that has never seen it, and ownership with it because a fresh
    /// graph has never been told who writes the device.
    pub fn reopen(&self, config: EngineConfig) -> Result<(), String> {
        self.locked()?
            .reopen_with_config(config)
            .map_err(|error| error.to_string())?;
        self.publish()?;
        self.claim_output()
    }

    /// Peak levels for this frame. Reading them resets nothing — these are the
    /// engine's own smoothed meters, shared with every other reader.
    pub fn levels(&self) -> Levels {
        use std::sync::atomic::Ordering;
        let peak = |left: u32, right: u32| f32_load(left).max(f32_load(right));
        Levels {
            input: peak(
                self.shared.live_input_peak_l.load(Ordering::Relaxed),
                self.shared.live_input_peak_r.load(Ordering::Relaxed),
            ),
            output: peak(
                self.shared.peak_l.load(Ordering::Relaxed),
                self.shared.peak_r.load(Ordering::Relaxed),
            ),
        }
    }

    /// What the capture side is actually doing, in the words the footer needs.
    ///
    /// Monitoring depends on three things the user cannot see — a capture
    /// stream being open, the input ring being live, and the engine agreeing
    /// that some track wants monitoring — and when any of them is false the
    /// symptom is identical: silence. Reporting them is the difference between
    /// "this app is broken" and "pick an input device".
    pub fn input_status(&self) -> InputStatus {
        use std::sync::atomic::Ordering;
        InputStatus {
            capture_open: self.shared.live_input_active.load(Ordering::Relaxed)
                && self.shared.input_ring.is_active(),
            monitoring: self.shared.monitor_enabled_any.load(Ordering::Relaxed),
            frames_captured: self.shared.input_frames_received.load(Ordering::Relaxed),
            last_error: self
                .locked()
                .ok()
                .and_then(|engine| engine.stats().last_error),
        }
    }

    /// Gain on the input bus before it reaches the mix — the "input" fader.
    pub fn set_input_gain(&self, gain: f32) -> Result<(), String> {
        self.locked()?
            .set_monitor_gain(gain.clamp(0.0, 2.0))
            .map_err(|error| error.to_string())
    }

    /// Whether local input monitoring is on.
    pub fn self_monitor(&self) -> bool {
        self.self_monitor.load(AtomicOrdering::Relaxed)
    }

    /// Hear this machine's own input, or stop.
    ///
    /// Implemented as a track like any other, because that is what it is: an
    /// audio track whose input is the interface and whose monitoring is on. The
    /// engine's input bus, its gain, and its meter are then the same ones every
    /// other part of this application already reads, rather than a second path
    /// that could disagree with them.
    pub fn set_self_monitor(&self, on: bool) -> Result<(), String> {
        if self.self_monitor.swap(on, AtomicOrdering::Relaxed) == on {
            return Ok(());
        }
        self.publish()
    }

    pub fn listeners(&self) -> Vec<Listener> {
        self.listening
            .lock()
            .map(|listening| listening.clone())
            .unwrap_or_default()
    }

    /// Start listening to a stream.
    ///
    /// Idempotent by device id: asking twice for the same performer is the same
    /// request, not a second copy of them summed into the mix.
    pub fn listen_to(
        &self,
        device_id: String,
        name: String,
        stream_channels: usize,
    ) -> Result<(), String> {
        {
            let mut listening = self.listeners_locked()?;
            if listening
                .iter()
                .any(|listener| listener.device_id == device_id)
            {
                return Ok(());
            }
            listening.push(Listener {
                device_id,
                name,
                channels: channels_for(stream_channels),
                volume: 1.0,
                muted: false,
            });
        }
        self.publish()
    }

    /// Stop listening to a stream and drop its track.
    pub fn stop_listening(&self, device_id: &str) -> Result<(), String> {
        {
            let mut listening = self.listeners_locked()?;
            let before = listening.len();
            listening.retain(|listener| listener.device_id != device_id);
            if listening.len() == before {
                return Ok(());
            }
        }
        self.publish()
    }

    pub fn set_volume(&self, device_id: &str, volume: f32) -> Result<(), String> {
        self.update(device_id, |listener| {
            listener.volume = volume.clamp(0.0, 2.0);
        })
    }

    pub fn set_muted(&self, device_id: &str, muted: bool) -> Result<(), String> {
        self.update(device_id, |listener| listener.muted = muted)
    }

    fn update(&self, device_id: &str, edit: impl FnOnce(&mut Listener)) -> Result<(), String> {
        {
            let mut listening = self.listeners_locked()?;
            let Some(listener) = listening
                .iter_mut()
                .find(|listener| listener.device_id == device_id)
            else {
                return Ok(());
            };
            edit(listener);
        }
        self.publish()
    }

    /// Hand the engine the project that describes who is being listened to.
    pub fn publish(&self) -> Result<(), String> {
        let mut tracks: Vec<EngineTrackSnapshot> = self
            .listeners_locked()?
            .iter()
            .enumerate()
            .map(|(index, listener)| listener_track(index, listener))
            .collect();
        if self.self_monitor() {
            tracks.push(self_monitor_track());
        }
        // The capture stream is opened by the project, not by the engine
        // config: `sync_live_input_stream` resolves a device from the monitored
        // track's own route and then from the project's preferred input, and a
        // project naming neither always opens the system default. Carrying the
        // settings choice here is what makes the Input device picker mean
        // anything at all.
        let engine = self.locked()?;
        let preferred_input = engine
            .config()
            .input_device
            .as_ref()
            .map(|device| device.raw_id().to_string());
        engine
            .load_project(room_project(tracks, preferred_input))
            .map_err(|error| error.to_string())
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, AudioEngine>, String> {
        self.engine
            .lock()
            .map_err(|_| "the audio engine lock is poisoned".to_string())
    }

    fn listeners_locked(&self) -> Result<std::sync::MutexGuard<'_, Vec<Listener>>, String> {
        self.listening
            .lock()
            .map_err(|_| "the listener list is poisoned".to_string())
    }
}

fn room_project(
    tracks: Vec<EngineTrackSnapshot>,
    preferred_input_device: Option<String>,
) -> EngineProjectSnapshot {
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "futureboard-jam".to_string(),
        project_root: None,
        preferred_input_device,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks,
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 0,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 256,
        },
    }
}

/// A performer as a track.
///
/// Monitoring is on and the transport is irrelevant: there is no timeline to
/// play, and the engine renders its realtime graph whether or not the transport
/// is running, so a routed stream is audible the moment it is published.
fn listener_track(index: usize, listener: &Listener) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: format!("jam-{index}"),
        track_type: "audio".to_string(),
        volume: listener.volume,
        pan: 0.0,
        muted: listener.muted,
        solo: false,
        // Never armed: this application records nothing, and an armed track
        // would ask the engine for a capture device it has no use for.
        armed: false,
        input_monitor: true,
        input_source: EngineTrackInputSourceSnapshot {
            device_id: Some(listener.device_id.clone()),
            channels: listener.channels.clone(),
        },
        preview_mode: "stereo".to_string(),
        output_track_id: None,
        inserts: Vec::new(),
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        builtin_soundfont_player: false,
        soundfont_path: None,
        soundfont_preset_bank: None,
        soundfont_preset_patch: None,
        soundfont_volume: 1.0,
        soundfont_reverb_chorus: true,
        soundfont_polyphony: 64,
        soundfont_envelope: Default::default(),
        soundfont_quality: Default::default(),
        solfege_engine: None,
    }
}

/// The local input, as a track.
///
/// `device_id: None` deliberately: the engine resolves an unpinned route to the
/// project's preferred input and then to the system default, which is the
/// answer that survives somebody unplugging an interface mid-session. Never
/// armed — monitoring is hearing, not recording.
fn self_monitor_track() -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: "jam-self".to_string(),
        track_type: "audio".to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: true,
        input_source: EngineTrackInputSourceSnapshot {
            device_id: None,
            channels: vec![0, 1],
        },
        preview_mode: "stereo".to_string(),
        output_track_id: None,
        inserts: Vec::new(),
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        builtin_soundfont_player: false,
        soundfont_path: None,
        soundfont_preset_bank: None,
        soundfont_preset_patch: None,
        soundfont_volume: 1.0,
        soundfont_reverb_chorus: true,
        soundfont_polyphony: 64,
        soundfont_envelope: Default::default(),
        soundfont_quality: Default::default(),
        solfege_engine: None,
    }
}

/// Which of a stream's channels a listening track takes.
///
/// A mono publisher is one channel that both sides hear; anything wider is
/// taken as its first pair, because this application mixes to a stereo monitor
/// and has no surround path to send the rest to.
pub fn channels_for(stream_channels: usize) -> Vec<u32> {
    match stream_channels {
        0 | 1 => vec![0],
        _ => vec![0, 1],
    }
}

#[cfg(test)]
mod tests {
    use super::{Listener, channels_for, listener_track, room_project, self_monitor_track};

    fn listener() -> Listener {
        Listener {
            device_id: "jam:str_1".to_string(),
            name: "Guitar".to_string(),
            channels: vec![0, 1],
            volume: 1.0,
            muted: false,
        }
    }

    /// The fold-down rule, pinned: a mono performer must not silently become
    /// one side of a stereo pair, and a wide one must not drag channels this
    /// application cannot route anywhere.
    #[test]
    fn a_stream_is_taken_as_mono_or_as_its_first_pair() {
        assert_eq!(channels_for(0), vec![0]);
        assert_eq!(channels_for(1), vec![0]);
        assert_eq!(channels_for(2), vec![0, 1]);
        assert_eq!(channels_for(16), vec![0, 1]);
    }

    /// The per-peer fader and mute are the track's own, so they have to reach
    /// the snapshot. A mixer whose controls are cosmetic is worse than one with
    /// no controls at all.
    #[test]
    fn a_listeners_level_and_mute_reach_the_track() {
        let quiet = Listener {
            volume: 0.25,
            muted: true,
            ..listener()
        };
        let track = listener_track(0, &quiet);
        assert_eq!(track.volume, 0.25);
        assert!(track.muted);
        assert_eq!(track.input_source.device_id.as_deref(), Some("jam:str_1"));
    }

    /// This application never records, and an armed track would make the engine
    /// open a capture device for a take nobody asked for.
    #[test]
    fn a_listening_track_is_monitored_and_never_armed() {
        let track = listener_track(3, &listener());
        assert!(track.input_monitor);
        assert!(!track.armed);
        assert_eq!(track.id, "jam-3");
    }

    /// Monitoring is hearing, not recording: an armed track would make the
    /// engine open a capture device for a take nobody asked for, and an
    /// unmonitored one would be silent, which is the whole feature.
    #[test]
    fn the_self_monitor_track_is_monitored_and_never_armed() {
        let track = self_monitor_track();
        assert!(track.input_monitor);
        assert!(!track.armed);
    }

    /// Unpinned on purpose: the engine resolves an empty device to the
    /// preferred input and then to the system default, which is what survives
    /// an interface being unplugged mid-session.
    #[test]
    fn the_self_monitor_track_follows_the_default_input() {
        let track = self_monitor_track();
        assert!(track.input_source.device_id.is_none());
        assert_eq!(track.input_source.channels, vec![0, 1]);
        assert!(
            !track.input_source.is_jam(),
            "the local input is hardware, never a room stream"
        );
    }

    /// The Input device picker has to reach the capture stream, and the only
    /// way it does is the project: `sync_live_input_stream` resolves a device
    /// from the monitored track's own route and then from the project's
    /// preferred input, so a project naming neither opens the system default
    /// however carefully the user chose something else.
    #[test]
    fn the_chosen_input_device_reaches_the_project() {
        let project = room_project(Vec::new(), Some("Studio 24c".to_string()));
        assert_eq!(
            project.preferred_input_device.as_deref(),
            Some("Studio 24c")
        );

        let unset = room_project(Vec::new(), None);
        assert!(
            unset.preferred_input_device.is_none(),
            "no choice must stay no choice rather than becoming an empty string"
        );
    }
}
