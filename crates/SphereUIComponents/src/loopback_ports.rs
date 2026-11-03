//! Track outputs as Audio Connections input ports.
//!
//! ```txt
//! Instrument track ──▶ trk:<track id> ──▶ Input menu ──▶ Audio track
//! ```
//!
//! This is what makes "record my VSTi to an audio track" an ordinary routing
//! choice rather than a bounce: the instrument shows up in the same Input menu
//! a hardware channel does, an audio track points at it, and arming that track
//! records the instrument's post-fader output.
//!
//! Modelled on [`crate::jam`]'s stream ports, and for the same reason: one
//! routing layer, with stable ids and non-destructive behaviour when the source
//! disappears, instead of a second private one beside it. The only difference
//! is the device-id prefix — see [`DirectAudio::loopback`].
//!
//! The source list is a process-wide snapshot the studio republishes whenever
//! the track list changes, because [`crate::audio_connections::current_available_ports`]
//! has no view of the project and should not grow one.

use std::sync::{OnceLock, RwLock};

use DirectAudio::loopback::loopback_device_id;

use crate::audio_connections::{AudioConnectionDirection, AvailablePort, AvailablePorts};
use crate::components::timeline::state::TrackType;

/// One track that can feed another track's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackSource {
    pub track_id: String,
    /// What the Input menu shows, e.g. `Piano (Instrument)`.
    pub display_name: String,
}

fn sources() -> &'static RwLock<Vec<LoopbackSource>> {
    static SOURCES: OnceLock<RwLock<Vec<LoopbackSource>>> = OnceLock::new();
    SOURCES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Replace the list of tracks offered as loopback sources.
///
/// Called from the studio whenever the track list or a track name changes. A
/// track that disappears simply stops being offered; any connection already
/// bound to it keeps its binding and reports the device as missing, which is
/// the same thing an unplugged interface does.
pub fn set_sources(next: Vec<LoopbackSource>) {
    if let Ok(mut guard) = sources().write() {
        if *guard != next {
            *guard = next;
        }
    }
}

/// The current source list.
pub fn snapshot() -> Vec<LoopbackSource> {
    sources()
        .read()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Which track kinds can be looped back.
///
/// Instrument and audio tracks carry a signal of their own. A bus, return or
/// group is deliberately included too: bouncing a submix to an audio track is
/// the same gesture and the engine treats every track alike. MIDI tracks are
/// excluded because they render no audio, so the route would be silent by
/// construction; the master is excluded because everything already ends up
/// there, so looping it back is a feedback path rather than a source.
pub fn is_loopback_source_kind(track_type: TrackType) -> bool {
    matches!(
        track_type,
        TrackType::Instrument
            | TrackType::Audio
            | TrackType::Bus
            | TrackType::Return
            | TrackType::Group
            | TrackType::Video
    )
}

/// Track outputs as input ports, one stereo pair per track.
pub fn available_ports() -> AvailablePorts {
    let sources = snapshot();
    let mut ports = Vec::with_capacity(sources.len() * 2);
    for source in &sources {
        let device_id = loopback_device_id(&source.track_id);
        for channel in 0..2u32 {
            ports.push(AvailablePort {
                device_id: device_id.clone(),
                device_name: source.display_name.clone(),
                port_name: if channel == 0 { "L" } else { "R" }.to_string(),
                port_index: channel,
                direction: AudioConnectionDirection::Input,
            });
        }
    }
    AvailablePorts {
        ports,
        ..AvailablePorts::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_becomes_a_stereo_input_device_under_its_own_id() {
        set_sources(vec![LoopbackSource {
            track_id: "trk_piano".into(),
            display_name: "Piano".into(),
        }]);
        let ports = available_ports();
        assert_eq!(ports.ports.len(), 2);
        assert!(ports
            .ports
            .iter()
            .all(|port| port.device_id == "trk:trk_piano"
                && port.direction == AudioConnectionDirection::Input));
        assert_eq!(ports.ports[0].port_name, "L");
        assert_eq!(ports.ports[1].port_name, "R");
        set_sources(Vec::new());
        assert!(available_ports().ports.is_empty());
    }

    /// A MIDI track renders no audio, so a route to one would be silent by
    /// construction — better never offered than offered and dead.
    #[test]
    fn only_track_kinds_that_render_audio_are_offered() {
        for kind in [
            TrackType::Instrument,
            TrackType::Audio,
            TrackType::Bus,
            TrackType::Return,
            TrackType::Group,
            TrackType::Video,
        ] {
            assert!(is_loopback_source_kind(kind), "{kind:?} should be offered");
        }
        // A MIDI track renders no audio, and the master is where everything
        // already ends up — looping it back is a feedback path, not a source.
        for kind in [TrackType::Midi, TrackType::Master] {
            assert!(
                !is_loopback_source_kind(kind),
                "{kind:?} must not be offered"
            );
        }
    }
}
