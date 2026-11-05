//! What this Studio sends the jam, as Audio Connections output ports.
//!
//! ```txt
//! Audio Connections (outputs bound to jam-send) ──▶ sends ──▶ publish
//! ```
//!
//! A remote performer is an *input* port, so a track can listen to them the way
//! it listens to an interface channel. This is the other direction: the jam is
//! an *output* device, with one stereo pair per tap the engine can already
//! take, and an enabled output bus bound to a pair **is** a publish. The bus's
//! name is the stream the room sees, its layout is the channel count, and
//! turning it off stops the stream — the same three gestures that already
//! govern a hardware output, applied to the network.
//!
//! Three taps, because those are the three the callback stages every block:
//!
//! * **Master** — the mix, before the Control Room touches it. The same signal
//!   an export gets.
//! * **Monitor** — after the Control Room: the signal that actually leaves for
//!   the monitoring output. "Send what I am hearing" is this one, not Master.
//! * **Live Input** — the interface pair, before any track. How a performer
//!   sends an instrument without arming a track for it.
//!
//! Per-track and multitrack sharing stay where they are, in the jam panel: a
//! track is not a static port, and inventing one per track would put a copy of
//! the track list inside the device inventory.
//!
//! Nothing here is called from an audio callback.

use std::collections::BTreeMap;

use DirectAudio::jam_bus::{
    is_jam_send_device, JAM_SEND_DEVICE_ID, JAM_SEND_PORTS, JAM_SEND_PORT_LIVE_INPUT,
    JAM_SEND_PORT_MASTER, JAM_SEND_PORT_MONITOR,
};

use crate::audio_connections::{
    AudioConnectionDirection, AudioConnectionRegistry, AudioConnectionStatus, AvailablePort,
    AvailablePorts,
};

/// The name the send device shows in a port menu.
pub const JAM_SEND_DEVICE_NAME: &str = "Audio Jam";

/// Which engine tap a send is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum JamSendTap {
    Master,
    /// The Control Room output — the mix *after* monitor inserts and the
    /// control processor, which is what the engineer is actually hearing.
    Monitor,
    LiveInput,
}

impl JamSendTap {
    /// The tap whose stereo pair contains this port.
    pub fn for_port(index: u32) -> Option<Self> {
        match index {
            i if i == JAM_SEND_PORT_MASTER || i == JAM_SEND_PORT_MASTER + 1 => Some(Self::Master),
            i if i == JAM_SEND_PORT_LIVE_INPUT || i == JAM_SEND_PORT_LIVE_INPUT + 1 => {
                Some(Self::LiveInput)
            }
            i if i == JAM_SEND_PORT_MONITOR || i == JAM_SEND_PORT_MONITOR + 1 => {
                Some(Self::Monitor)
            }
            _ => None,
        }
    }

    /// The port name shown in the Audio Connections window.
    pub fn port_name(index: u32) -> String {
        let side = if index % 2 == 0 { "L" } else { "R" };
        match Self::for_port(index) {
            Some(Self::Master) => format!("Master {side}"),
            Some(Self::Monitor) => format!("Monitor {side}"),
            Some(Self::LiveInput) => format!("Live Input {side}"),
            None => format!("Port {}", index + 1),
        }
    }
}

/// One output bus that is a jam send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JamSend {
    /// The Audio Connection id, which is what a send is keyed by: the name can
    /// change and the stream follows it, but the identity is the bus.
    pub connection_id: String,
    /// The bus name, which is the stream name in the room.
    pub name: String,
    pub tap: JamSendTap,
    /// 1 or 2. A mono bus bound to one side of a pair publishes that side.
    pub channels: usize,
}

/// The send device's ports, for the Outputs tab and the port menus.
pub fn available_ports() -> AvailablePorts {
    AvailablePorts {
        ports: (0..JAM_SEND_PORTS)
            .map(|index| AvailablePort {
                device_id: JAM_SEND_DEVICE_ID.to_string(),
                device_name: JAM_SEND_DEVICE_NAME.to_string(),
                port_name: JamSendTap::port_name(index),
                port_index: index,
                direction: AudioConnectionDirection::Output,
            })
            .collect(),
        ..AvailablePorts::default()
    }
}

/// Every enabled output bus that is bound, entirely, to one jam tap.
///
/// A bus with one channel on the master pair and one on the live-input pair is
/// not a send: there is no stream it could describe, so it is skipped rather
/// than guessed at. The registry already reports it as a conflict for the user.
pub fn sends(registry: &AudioConnectionRegistry) -> Vec<JamSend> {
    let mut out = Vec::new();
    for connection in registry.by_direction(AudioConnectionDirection::Output) {
        if !connection.enabled || connection.status == AudioConnectionStatus::Disabled {
            continue;
        }
        if connection.port_bindings.is_empty()
            || !connection
                .port_bindings
                .iter()
                .all(|binding| is_jam_send_device(&binding.physical_port_id.device_id))
        {
            continue;
        }
        let mut taps = connection
            .port_bindings
            .iter()
            .filter_map(|binding| JamSendTap::for_port(binding.physical_port_id.port_index));
        let Some(tap) = taps.next() else {
            continue;
        };
        if taps.any(|other| other != tap) {
            continue;
        }
        out.push(JamSend {
            connection_id: connection.id.as_str().to_string(),
            name: connection.name.clone(),
            tap,
            channels: connection.channel_layout.channel_count().clamp(1, 2),
        });
    }
    out.sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
    out
}

/// Sends keyed by connection id, the shape the controller diffs against.
pub fn by_connection(sends: Vec<JamSend>) -> BTreeMap<String, JamSend> {
    sends
        .into_iter()
        .map(|send| (send.connection_id.clone(), send))
        .collect()
}

/// Push the project's output routing at the jam session.
///
/// Idempotent: the controller diffs by connection, so a routing recompile that
/// moved a hardware port costs nothing here.
///
/// Runs on the UI thread, so it never waits for the jam controller: the sends
/// are queued and applied as soon as the controller is free.
pub fn sync(registry: &AudioConnectionRegistry) {
    super::queue_sends(sends(registry));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::{AudioConnection, ChannelLayout};

    fn send_bus(name: &str, first_port: u32, layout: ChannelLayout) -> AudioConnection {
        AudioConnection::new(name, AudioConnectionDirection::Output, layout).bind_consecutive(
            JAM_SEND_DEVICE_ID,
            first_port,
            JamSendTap::port_name,
        )
    }

    #[test]
    fn the_send_device_offers_every_tap_as_a_stereo_pair() {
        let ports = available_ports();
        assert_eq!(ports.ports.len(), 6);
        assert!(ports
            .ports
            .iter()
            .all(|port| port.direction == AudioConnectionDirection::Output));
        assert_eq!(ports.ports[0].port_name, "Master L");
        assert_eq!(ports.ports[1].port_name, "Master R");
        assert_eq!(ports.ports[2].port_name, "Live Input L");
        assert_eq!(ports.ports[3].port_name, "Live Input R");
        assert_eq!(ports.ports[4].port_name, "Monitor L");
        assert_eq!(ports.ports[5].port_name, "Monitor R");
    }

    /// Master and Monitor are different signals, and a send bound to the
    /// Control Room pair has to resolve to the Control Room tap — binding it to
    /// Master would publish the pre-monitor mix under a name that promises what
    /// the engineer is hearing.
    #[test]
    fn the_monitor_pair_resolves_to_the_control_room_tap() {
        assert_eq!(JamSendTap::for_port(4), Some(JamSendTap::Monitor));
        assert_eq!(JamSendTap::for_port(5), Some(JamSendTap::Monitor));
        assert_eq!(JamSendTap::for_port(0), Some(JamSendTap::Master));
        assert_eq!(JamSendTap::for_port(6), None);
    }

    #[test]
    fn an_enabled_output_bus_on_a_tap_is_a_send_named_after_the_bus() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(send_bus(
            "Studio Mix",
            JAM_SEND_PORT_MASTER,
            ChannelLayout::Stereo,
        ));
        registry.add(send_bus(
            "My Guitar",
            JAM_SEND_PORT_LIVE_INPUT,
            ChannelLayout::Mono,
        ));
        let sends = sends(&registry);
        assert_eq!(sends.len(), 2);
        let mix = sends.iter().find(|s| s.name == "Studio Mix").expect("mix");
        assert_eq!(mix.tap, JamSendTap::Master);
        assert_eq!(mix.channels, 2);
        let guitar = sends
            .iter()
            .find(|s| s.name == "My Guitar")
            .expect("guitar");
        assert_eq!(guitar.tap, JamSendTap::LiveInput);
        assert_eq!(guitar.channels, 1);
    }

    #[test]
    fn a_hardware_output_is_never_a_send() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(
            AudioConnection::new(
                "Main Out",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("focusrite", 0, |i| format!("Output {}", i + 1)),
        );
        assert!(sends(&registry).is_empty());
    }

    #[test]
    fn disabling_the_bus_stops_the_send() {
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(send_bus(
            "Studio Mix",
            JAM_SEND_PORT_MASTER,
            ChannelLayout::Stereo,
        ));
        assert_eq!(sends(&registry).len(), 1);
        registry.set_enabled(&id, false);
        assert!(sends(&registry).is_empty());
    }

    #[test]
    fn a_bus_straddling_two_taps_describes_no_stream() {
        let mut registry = AudioConnectionRegistry::new();
        // Left on the master pair, right on the live-input pair.
        registry.add(send_bus(
            "Confused",
            JAM_SEND_PORT_MASTER + 1,
            ChannelLayout::Stereo,
        ));
        assert!(sends(&registry).is_empty());
    }

    #[test]
    fn an_input_bus_on_the_send_device_is_ignored() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(
            AudioConnection::new(
                "Wrong Way",
                AudioConnectionDirection::Input,
                ChannelLayout::Stereo,
            )
            .bind_consecutive(JAM_SEND_DEVICE_ID, 0, JamSendTap::port_name),
        );
        assert!(sends(&registry).is_empty());
    }
}
