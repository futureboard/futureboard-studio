//! What this Studio asks the jam to send it.
//!
//! ```txt
//! Audio Connections (inputs bound to jam:<stream>) ──▶ routed set ──▶ subscribe
//! ```
//!
//! Egress was always explicit — a Studio publishes what it chooses. Ingress was
//! not: the server sent every participant every stream it could decode, and a
//! Studio decoded all of it whether or not anything read it.
//!
//! That waste is not theoretical, and it is worse here than anywhere else in
//! the room. A remote stream only becomes audible in Studio by way of an Audio
//! Connection: the sink writes it into a jam bus input slot, and the audio
//! callback reads that slot **only** when some track's input connection binds
//! `jam:<stream_id>`. A stream nobody routed was therefore received, reordered,
//! decoded, resampled and thrown away — at roughly 1.5 Mbit/s per 48 kHz stereo
//! stream, on the uplink that is already the constraint in a jam.
//!
//! So the routing registry decides. Every enabled input connection that binds a
//! jam port is a stream this Studio wants; everything else in the room is
//! announced, listed in the panel, and not sent. A stream that is wanted but not
//! published yet stays wanted — a track bound to a performer who has not started
//! is a track waiting, and the jam client attaches it when they publish.
//!
//! Nothing here is called from an audio callback.

use std::collections::BTreeSet;

use DirectAudio::jam_bus::jam_stream_id;

use crate::audio_connections::{
    AudioConnectionDirection, AudioConnectionRegistry, AudioConnectionStatus,
};

/// The jam stream ids some enabled input connection binds.
///
/// Read from the registry rather than from the compiled routing snapshot,
/// because the two answer different questions: the snapshot is what the audio
/// callback can reach *right now*, and a jam stream whose publisher has not
/// joined resolves to nothing there. What ingress needs is the intent — which
/// performer this project is pointed at — so that the stream is subscribed the
/// moment it appears rather than after the user notices and re-routes.
pub fn routed_streams(registry: &AudioConnectionRegistry) -> Vec<String> {
    let mut out = BTreeSet::new();
    for connection in registry.by_direction(AudioConnectionDirection::Input) {
        // A disabled bus is a deliberate "not now", and paying for its audio
        // would make disabling it pointless. `Disabled` is checked as well as
        // the flag because the status is what the panel shows the user.
        if !connection.enabled || connection.status == AudioConnectionStatus::Disabled {
            continue;
        }
        for binding in &connection.port_bindings {
            if let Some(stream) = jam_stream_id(&binding.physical_port_id.device_id) {
                out.insert(stream.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Push the project's routing intent at the jam session.
///
/// Idempotent and cheap: the controller diffs against what it last asked for,
/// so calling this on every routing recompile — which is what the caller does —
/// costs nothing when the routing did not touch a jam port.
///
/// Runs on the UI thread, so it never waits for the jam controller: the intent
/// is queued and applied as soon as the controller is free (see
/// `PendingRouting` in the jam module).
pub fn sync(registry: &AudioConnectionRegistry) {
    super::queue_routed_streams(routed_streams(registry));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::{AudioConnection, ChannelLayout};
    use DirectAudio::jam_bus::jam_device_id;

    fn jam_input(name: &str, stream: &str) -> AudioConnection {
        AudioConnection::new(name, AudioConnectionDirection::Input, ChannelLayout::Stereo)
            .bind_consecutive(jam_device_id(stream), 0, |index| match index {
                0 => "L".to_string(),
                _ => "R".to_string(),
            })
    }

    fn hardware_input(name: &str) -> AudioConnection {
        AudioConnection::new(name, AudioConnectionDirection::Input, ChannelLayout::Stereo)
            .bind_consecutive("focusrite", 0, |index| format!("Input {}", index + 1))
    }

    #[test]
    fn only_jam_ports_become_a_subscription() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(jam_input("Guitar", "str_1"));
        registry.add(hardware_input("Stereo Input 1-2"));
        assert_eq!(routed_streams(&registry), vec!["str_1".to_string()]);
    }

    #[test]
    fn a_disabled_bus_is_not_paid_for() {
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(jam_input("Guitar", "str_1"));
        assert_eq!(routed_streams(&registry), vec!["str_1".to_string()]);
        registry.set_enabled(&id, false);
        assert!(
            routed_streams(&registry).is_empty(),
            "disabling a bus has to stop the bandwidth, or disabling it means nothing"
        );
    }

    #[test]
    fn two_tracks_on_one_performer_ask_for_one_stream() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(jam_input("Guitar", "str_1"));
        registry.add(jam_input("Guitar Double", "str_1"));
        assert_eq!(
            routed_streams(&registry),
            vec!["str_1".to_string()],
            "two tracks listening to one performer is one subscription, not two"
        );
    }

    #[test]
    fn an_output_bus_never_produces_a_subscription() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(
            AudioConnection::new(
                "Main Output",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive(jam_device_id("str_1"), 0, |index| format!("{index}")),
        );
        assert!(routed_streams(&registry).is_empty());
    }
}
