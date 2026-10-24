//! v33 → v34 track routing migration.
//!
//! Project format v33 and older stored one *combined* `TrackInputRouting`
//! union carrying both audio device/channel routing and a MIDI device. v34
//! splits that into two independent, authoritative fields:
//!
//! ```txt
//! v33:  routing.input : TrackInputRouting {None, AllInputs, AudioDeviceChannel,
//!                                          AudioDeviceChannels, MidiDevice}
//!       routing.midi_input : TrackMidiInputRouting
//!
//! v34:  routing.audio_input_connection_id : Option<AudioConnectionId>
//!       routing.midi_input                : TrackMidiInputRouting
//! ```
//!
//! [`LegacyTrackInputRouting`] exists **only** here. It is decoded from an old
//! file, consumed by [`migrate_track_routing`], and dropped — it is never part
//! of the runtime `TrackState` API, so there is no second live audio-input
//! field after migration.
//!
//! **Status:** the conversion rules below are complete and tested, but the
//! v34 field split is not yet wired into `TrackRoutingState`, the binary
//! codec, or the 106 call sites that still read the combined
//! `TrackInputRouting`. Nothing calls [`migrate_track_routing`] yet.
//!
//! Audio routing becomes a project-local [`AudioConnection`]. Several tracks
//! sharing one legacy source share one generated connection, keyed by
//! [`LegacyAudioSourceKey`]. Ids are minted once during migration and written
//! on the next save, so a v34 project never regenerates them.

use crate::audio_connections::{
    AudioConnection, AudioConnectionDirection, AudioConnectionId, AudioConnectionRegistry,
    AudioPortId, AvailablePorts, ChannelLayout,
};
use crate::components::timeline::timeline_state::TrackMidiInputRouting;
use std::collections::HashMap;

/// The v33 combined routing union. Migration-only — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyTrackInputRouting {
    None,
    AllInputs,
    AudioDeviceChannel {
        device_id: String,
        channel: u32,
    },
    AudioDeviceChannels {
        device_id: String,
        channels: Vec<u32>,
    },
    MidiDevice {
        device_id: String,
    },
}

impl LegacyTrackInputRouting {
    /// The audio device/channel mapping this legacy value describes, if any.
    /// `AllInputs` carries no concrete device, so it cannot become a
    /// connection — it migrates to No Input rather than guessing a device.
    fn audio_source(&self) -> Option<LegacyAudioSourceKey> {
        match self {
            Self::AudioDeviceChannel { device_id, channel } => Some(LegacyAudioSourceKey {
                device_id: device_id.clone(),
                channels: vec![*channel],
            }),
            Self::AudioDeviceChannels {
                device_id,
                channels,
            } if !channels.is_empty() => Some(LegacyAudioSourceKey {
                device_id: device_id.clone(),
                channels: channels.clone(),
            }),
            _ => None,
        }
    }

    fn midi_device(&self) -> Option<&str> {
        match self {
            Self::MidiDevice { device_id } => Some(device_id.as_str()),
            _ => None,
        }
    }

    /// Description used in migration warnings.
    pub fn describe(&self) -> String {
        match self {
            Self::None => "None".to_string(),
            Self::AllInputs => "All Inputs".to_string(),
            Self::AudioDeviceChannel { device_id, channel } => {
                format!("{device_id} ch {}", channel + 1)
            }
            Self::AudioDeviceChannels {
                device_id,
                channels,
            } => {
                let labels: Vec<_> = channels.iter().map(|c| (c + 1).to_string()).collect();
                format!("{device_id} ch {}", labels.join("+"))
            }
            Self::MidiDevice { device_id } => format!("MIDI device {device_id}"),
        }
    }
}

/// Identity of one legacy audio source. Two tracks whose legacy routing has the
/// same device **and** the same ordered channel list share one generated
/// connection; a different device or a different channel list creates another.
///
/// Channel order is part of the key, so a `[0, 1]` source and a `[1, 0]` source
/// stay distinct rather than collapsing and silently swapping Left/Right.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LegacyAudioSourceKey {
    pub device_id: String,
    pub channels: Vec<u32>,
}

impl LegacyAudioSourceKey {
    fn layout(&self) -> ChannelLayout {
        match self.channels.len() {
            1 => ChannelLayout::Mono,
            2 => ChannelLayout::Stereo,
            n => ChannelLayout::Custom { channels: n },
        }
    }

    /// Understandable generated name, e.g. `"Input 1"` or `"Stereo Input 1-2"`.
    ///
    /// Delegates to the shared helper so migration and the Inspector/Add Track
    /// bridge cannot drift — and so a migrated bus gets a *logical* name. The
    /// hardware endpoint is shown in the Audio Device column, never folded
    /// into the bus name.
    fn generated_name(&self, device_name: Option<&str>) -> String {
        crate::audio_connections::generated_input_name(&self.channels, device_name)
    }
}

/// One structured problem found while migrating. Surfaced as a warning; never
/// a load failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingMigrationWarning {
    /// The legacy union carried a MIDI device *and* the dedicated `midi_input`
    /// field already held a different, non-default assignment. The dedicated
    /// field wins; the user is told rather than having one silently chosen.
    ConflictingMidiInput {
        track_id: String,
        track_name: String,
        legacy_routing: String,
        retained_routing: String,
        source_version: u32,
    },
}

impl RoutingMigrationWarning {
    pub fn message(&self) -> String {
        match self {
            Self::ConflictingMidiInput {
                track_name,
                legacy_routing,
                retained_routing,
                source_version,
                ..
            } => format!(
                "Track \"{track_name}\": conflicting legacy MIDI input routing was found \
                 (project v{source_version}). The dedicated MIDI input assignment was \
                 retained. Legacy: {legacy_routing}. Retained: {retained_routing}."
            ),
        }
    }
}

/// Per-track migration input.
#[derive(Debug, Clone)]
pub struct LegacyTrackRouting {
    pub track_id: String,
    pub track_name: String,
    pub legacy_input: LegacyTrackInputRouting,
    /// The dedicated field as decoded from the same file.
    pub midi_input: TrackMidiInputRouting,
    /// The code-defined default for *this track type*. There is no single
    /// global default: `TrackRoutingState::for_track_type` uses `None` for
    /// audio tracks and `AllInputs` for MIDI/instrument tracks, so the
    /// "is it still default?" test in Case A has to be made against the right
    /// one or an untouched MIDI track would look explicitly assigned.
    pub midi_input_default: TrackMidiInputRouting,
}

/// Per-track migration result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigratedTrackRouting {
    pub track_id: String,
    pub audio_input_connection_id: Option<AudioConnectionId>,
    pub midi_input: TrackMidiInputRouting,
}

/// Everything one migration pass produced.
#[derive(Debug, Clone, Default)]
pub struct RoutingMigrationResult {
    /// Connections generated from legacy audio routing, to be merged into the
    /// project registry.
    pub generated_connections: Vec<AudioConnection>,
    pub tracks: Vec<MigratedTrackRouting>,
    pub warnings: Vec<RoutingMigrationWarning>,
}

/// Migrate a v33 (or older) project's track routing to the v34 split model.
///
/// `ports` is used only to *name* generated connections and to stamp their
/// initial status; a device that is absent still produces a connection, marked
/// `DeviceMissing` by the caller's `revalidate`, so no legacy assignment is
/// discarded.
pub fn migrate_track_routing(
    tracks: &[LegacyTrackRouting],
    ports: &AvailablePorts,
    source_version: u32,
) -> RoutingMigrationResult {
    let mut result = RoutingMigrationResult::default();
    // Reuse one generated connection per distinct legacy source.
    let mut by_source: HashMap<LegacyAudioSourceKey, AudioConnectionId> = HashMap::new();

    for track in tracks {
        // ── Audio ───────────────────────────────────────────────────────────
        let audio_input_connection_id = match track.legacy_input.audio_source() {
            None => None,
            Some(key) => Some(match by_source.get(&key) {
                Some(existing) => existing.clone(),
                None => {
                    let device_name = ports.device_name(&key.device_id);
                    let mut connection = AudioConnection::new(
                        key.generated_name(device_name),
                        AudioConnectionDirection::Input,
                        key.layout(),
                    );
                    connection.device_id = Some(key.device_id.clone());
                    connection.port_bindings = key
                        .channels
                        .iter()
                        .enumerate()
                        .map(|(logical_channel, physical)| {
                            // Prefer the live port name so a later reconnect
                            // resolves by name; fall back to a synthesized one.
                            let port_name = ports
                                .ports_for(&key.device_id, AudioConnectionDirection::Input)
                                .into_iter()
                                .find(|port| port.port_index == *physical)
                                .map(|port| port.port_name.clone())
                                .unwrap_or_else(|| format!("Input {}", physical + 1));
                            crate::audio_connections::AudioPortBinding {
                                logical_channel,
                                physical_port_id: AudioPortId::new(
                                    &key.device_id,
                                    port_name,
                                    *physical,
                                ),
                            }
                        })
                        .collect();
                    let id = connection.id.clone();
                    result.generated_connections.push(connection);
                    by_source.insert(key, id.clone());
                    id
                }
            }),
        };

        // ── MIDI ────────────────────────────────────────────────────────────
        let midi_input = match track.legacy_input.midi_device() {
            // Legacy value carried no MIDI device: the dedicated field is
            // already authoritative and is left exactly as decoded.
            None => track.midi_input.clone(),
            Some(legacy_device) => {
                let legacy_equivalent = TrackMidiInputRouting::MidiDevice {
                    device_id: legacy_device.to_string(),
                };
                if track.midi_input == legacy_equivalent {
                    // Case B — the same routing recorded twice. Deduplicate
                    // silently; nothing was lost.
                    track.midi_input.clone()
                } else if track.midi_input == track.midi_input_default {
                    // Case A — nothing explicit in the dedicated field, so the
                    // legacy assignment is the user's real intent.
                    legacy_equivalent
                } else {
                    // Case C — two different explicit assignments. The
                    // dedicated field wins and the user is told.
                    result
                        .warnings
                        .push(RoutingMigrationWarning::ConflictingMidiInput {
                            track_id: track.track_id.clone(),
                            track_name: track.track_name.clone(),
                            legacy_routing: track.legacy_input.describe(),
                            retained_routing: track.midi_input.label(),
                            source_version,
                        });
                    track.midi_input.clone()
                }
            }
        };

        result.tracks.push(MigratedTrackRouting {
            track_id: track.track_id.clone(),
            audio_input_connection_id,
            midi_input,
        });
    }

    result
}

/// Merge migration output into a project registry, then revalidate so every
/// generated connection gets a real status (including `DeviceMissing`).
pub fn merge_generated_connections(
    registry: &mut AudioConnectionRegistry,
    generated: Vec<AudioConnection>,
    ports: &AvailablePorts,
) {
    for connection in generated {
        // `add` disambiguates names but preserves the already-minted id, so
        // track references stay valid.
        let id = connection.id.clone();
        let mut connection = connection;
        connection.id = id;
        registry.add(connection);
    }
    registry.revalidate(ports);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::AudioConnectionStatus;

    fn ports() -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "System Audio Device", 4, 4)
    }

    /// Audio-track shaped: its code-defined MIDI default is `None`.
    fn track(
        id: &str,
        legacy: LegacyTrackInputRouting,
        midi: TrackMidiInputRouting,
    ) -> LegacyTrackRouting {
        LegacyTrackRouting {
            track_id: id.to_string(),
            track_name: format!("Track {id}"),
            legacy_input: legacy,
            midi_input: midi,
            midi_input_default: TrackMidiInputRouting::None,
        }
    }

    /// MIDI/instrument shaped: its code-defined MIDI default is `AllInputs`.
    fn midi_track(
        id: &str,
        legacy: LegacyTrackInputRouting,
        midi: TrackMidiInputRouting,
    ) -> LegacyTrackRouting {
        LegacyTrackRouting {
            midi_input_default: TrackMidiInputRouting::AllInputs,
            ..track(id, legacy, midi)
        }
    }

    fn migrate(tracks: &[LegacyTrackRouting]) -> RoutingMigrationResult {
        migrate_track_routing(tracks, &ports(), 33)
    }

    // ── Audio ───────────────────────────────────────────────────────────────

    #[test]
    fn legacy_mono_audio_input_becomes_a_mono_connection() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::AudioDeviceChannel {
                device_id: "dev-1".into(),
                channel: 0,
            },
            TrackMidiInputRouting::None,
        )];
        let result = migrate(&tracks);

        assert_eq!(result.generated_connections.len(), 1);
        let connection = &result.generated_connections[0];
        assert_eq!(connection.channel_layout, ChannelLayout::Mono);
        // Logical name; the endpoint stays in the Audio Device column.
        assert_eq!(connection.name, "Input 1");
        assert_eq!(connection.port_bindings.len(), 1);
        assert_eq!(connection.port_bindings[0].physical_port_id.port_index, 0);
        assert_eq!(
            result.tracks[0].audio_input_connection_id.as_ref(),
            Some(&connection.id)
        );
    }

    #[test]
    fn legacy_stereo_input_preserves_left_right_ordering() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::AudioDeviceChannels {
                device_id: "dev-1".into(),
                channels: vec![2, 3],
            },
            TrackMidiInputRouting::None,
        )];
        let result = migrate(&tracks);

        let connection = &result.generated_connections[0];
        assert_eq!(connection.channel_layout, ChannelLayout::Stereo);
        assert_eq!(
            connection.binding(0).unwrap().physical_port_id.port_index,
            2
        );
        assert_eq!(
            connection.binding(1).unwrap().physical_port_id.port_index,
            3
        );
        // A logical name — the endpoint description stays in the device column.
        assert_eq!(connection.name, "Stereo Input 3-4");
    }

    /// A reversed channel list is a *different* source, not the same one —
    /// collapsing them would silently swap Left and Right.
    #[test]
    fn reversed_stereo_channels_are_a_distinct_source() {
        let tracks = vec![
            track(
                "t1",
                LegacyTrackInputRouting::AudioDeviceChannels {
                    device_id: "dev-1".into(),
                    channels: vec![0, 1],
                },
                TrackMidiInputRouting::None,
            ),
            track(
                "t2",
                LegacyTrackInputRouting::AudioDeviceChannels {
                    device_id: "dev-1".into(),
                    channels: vec![1, 0],
                },
                TrackMidiInputRouting::None,
            ),
        ];
        let result = migrate(&tracks);
        assert_eq!(result.generated_connections.len(), 2);
        assert_ne!(
            result.tracks[0].audio_input_connection_id,
            result.tracks[1].audio_input_connection_id
        );
    }

    #[test]
    fn tracks_sharing_one_legacy_source_reuse_one_connection() {
        let source = LegacyTrackInputRouting::AudioDeviceChannel {
            device_id: "dev-1".into(),
            channel: 1,
        };
        let tracks = vec![
            track("t1", source.clone(), TrackMidiInputRouting::None),
            track("t2", source.clone(), TrackMidiInputRouting::None),
            track("t3", source, TrackMidiInputRouting::None),
        ];
        let result = migrate(&tracks);

        assert_eq!(result.generated_connections.len(), 1);
        let id = result.tracks[0].audio_input_connection_id.clone();
        assert!(result
            .tracks
            .iter()
            .all(|t| t.audio_input_connection_id == id));
    }

    #[test]
    fn different_legacy_sources_create_different_connections() {
        let tracks = vec![
            track(
                "t1",
                LegacyTrackInputRouting::AudioDeviceChannel {
                    device_id: "dev-1".into(),
                    channel: 0,
                },
                TrackMidiInputRouting::None,
            ),
            track(
                "t2",
                LegacyTrackInputRouting::AudioDeviceChannel {
                    device_id: "dev-1".into(),
                    channel: 1,
                },
                TrackMidiInputRouting::None,
            ),
            track(
                "t3",
                LegacyTrackInputRouting::AudioDeviceChannel {
                    device_id: "dev-2".into(),
                    channel: 0,
                },
                TrackMidiInputRouting::None,
            ),
        ];
        let result = migrate(&tracks);
        assert_eq!(result.generated_connections.len(), 3);
    }

    #[test]
    fn a_missing_legacy_device_is_preserved_as_device_missing() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::AudioDeviceChannel {
                device_id: "unplugged-interface".into(),
                channel: 0,
            },
            TrackMidiInputRouting::None,
        )];
        let result = migrate(&tracks);
        assert_eq!(result.generated_connections.len(), 1);

        let mut registry = AudioConnectionRegistry::new();
        merge_generated_connections(&mut registry, result.generated_connections, &ports());

        let id = result.tracks[0]
            .audio_input_connection_id
            .clone()
            .expect("assignment preserved");
        let connection = registry.get(&id).expect("connection preserved");
        assert_eq!(connection.status, AudioConnectionStatus::DeviceMissing);
        assert_eq!(connection.name, "Input 1", "unknown device gets no prefix");
        assert_eq!(
            connection.port_bindings.len(),
            1,
            "the legacy mapping is kept so the device can come back"
        );
    }

    #[test]
    fn legacy_none_and_all_inputs_leave_the_track_unassigned() {
        let tracks = vec![
            track(
                "t1",
                LegacyTrackInputRouting::None,
                TrackMidiInputRouting::None,
            ),
            track(
                "t2",
                LegacyTrackInputRouting::AllInputs,
                TrackMidiInputRouting::None,
            ),
        ];
        let result = migrate(&tracks);
        assert!(result.generated_connections.is_empty());
        assert!(result
            .tracks
            .iter()
            .all(|t| t.audio_input_connection_id.is_none()));
    }

    // ── MIDI conflict rules ─────────────────────────────────────────────────

    #[test]
    fn case_a_legacy_midi_migrates_when_the_dedicated_field_is_default() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::MidiDevice {
                device_id: "MPK Mini".into(),
            },
            TrackMidiInputRouting::None,
        )];
        let result = migrate(&tracks);

        assert_eq!(
            result.tracks[0].midi_input,
            TrackMidiInputRouting::MidiDevice {
                device_id: "MPK Mini".into()
            }
        );
        assert!(result.warnings.is_empty());
        assert!(result.tracks[0].audio_input_connection_id.is_none());
    }

    #[test]
    fn case_b_identical_assignments_deduplicate_without_a_warning() {
        let device = TrackMidiInputRouting::MidiDevice {
            device_id: "MPK Mini".into(),
        };
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::MidiDevice {
                device_id: "MPK Mini".into(),
            },
            device.clone(),
        )];
        let result = migrate(&tracks);

        assert_eq!(result.tracks[0].midi_input, device);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn case_c_conflicting_assignments_retain_the_dedicated_field() {
        let dedicated = TrackMidiInputRouting::MidiDevice {
            device_id: "Keystation".into(),
        };
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::MidiDevice {
                device_id: "MPK Mini".into(),
            },
            dedicated.clone(),
        )];
        let result = migrate(&tracks);

        assert_eq!(
            result.tracks[0].midi_input, dedicated,
            "the dedicated field must win"
        );
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn case_c_emits_a_structured_warning_naming_both_assignments() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::MidiDevice {
                device_id: "MPK Mini".into(),
            },
            TrackMidiInputRouting::MidiDevice {
                device_id: "Keystation".into(),
            },
        )];
        let result = migrate_track_routing(&tracks, &ports(), 33);

        match &result.warnings[0] {
            RoutingMigrationWarning::ConflictingMidiInput {
                track_id,
                track_name,
                legacy_routing,
                retained_routing,
                source_version,
            } => {
                assert_eq!(track_id, "t1");
                assert_eq!(track_name, "Track t1");
                assert!(legacy_routing.contains("MPK Mini"));
                assert!(retained_routing.contains("Keystation"));
                assert_eq!(*source_version, 33);
            }
        }
        let message = result.warnings[0].message();
        assert!(message.contains("dedicated MIDI input assignment was retained"));
        assert!(message.contains("v33"));
    }

    /// `AllInputs` is non-default *for an audio track*, so it must win over a
    /// legacy device rather than being treated as "unset".
    #[test]
    fn a_non_default_all_inputs_dedicated_field_still_wins() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::MidiDevice {
                device_id: "MPK Mini".into(),
            },
            TrackMidiInputRouting::AllInputs,
        )];
        let result = migrate(&tracks);
        assert_eq!(
            result.tracks[0].midi_input,
            TrackMidiInputRouting::AllInputs
        );
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn legacy_none_leaves_dedicated_midi_routing_unchanged() {
        let dedicated = TrackMidiInputRouting::MidiDevice {
            device_id: "Keystation".into(),
        };
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::None,
            dedicated.clone(),
        )];
        let result = migrate(&tracks);

        assert_eq!(result.tracks[0].midi_input, dedicated);
        assert!(result.tracks[0].audio_input_connection_id.is_none());
        assert!(result.warnings.is_empty());
    }

    // ── Coexistence ─────────────────────────────────────────────────────────

    #[test]
    fn audio_and_midi_routing_can_coexist_on_one_track() {
        let dedicated = TrackMidiInputRouting::MidiDevice {
            device_id: "Keystation".into(),
        };
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::AudioDeviceChannels {
                device_id: "dev-1".into(),
                channels: vec![0, 1],
            },
            dedicated.clone(),
        )];
        let result = migrate(&tracks);

        assert!(
            result.tracks[0].audio_input_connection_id.is_some(),
            "the audio side migrates"
        );
        assert_eq!(
            result.tracks[0].midi_input, dedicated,
            "and the MIDI side is untouched"
        );
        assert!(result.warnings.is_empty());
    }

    // ── Id stability ────────────────────────────────────────────────────────

    #[test]
    fn generated_ids_survive_a_merge_into_the_registry() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::AudioDeviceChannel {
                device_id: "dev-1".into(),
                channel: 0,
            },
            TrackMidiInputRouting::None,
        )];
        let result = migrate(&tracks);
        let assigned = result.tracks[0].audio_input_connection_id.clone().unwrap();

        let mut registry = AudioConnectionRegistry::new();
        merge_generated_connections(&mut registry, result.generated_connections, &ports());

        assert!(
            registry.get(&assigned).is_some(),
            "the id minted during migration must be the id stored in the registry"
        );
    }

    /// Migration runs only for old files. Two independent migrations of the
    /// same legacy data mint different ids — which is exactly why the ids must
    /// be persisted on the next save rather than recomputed on every load.
    #[test]
    fn ids_are_minted_once_so_they_must_be_persisted() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::AudioDeviceChannel {
                device_id: "dev-1".into(),
                channel: 0,
            },
            TrackMidiInputRouting::None,
        )];
        let first = migrate(&tracks);
        let second = migrate(&tracks);
        assert_ne!(
            first.tracks[0].audio_input_connection_id, second.tracks[0].audio_input_connection_id,
            "re-migrating regenerates ids; v34 files must carry them instead"
        );
    }

    #[test]
    fn a_name_collision_between_generated_connections_keeps_both_ids_valid() {
        let mut registry = AudioConnectionRegistry::new();
        let existing = AudioConnection::new(
            "Input 1",
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
        );
        registry.add(existing);

        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::AudioDeviceChannel {
                device_id: "dev-1".into(),
                channel: 0,
            },
            TrackMidiInputRouting::None,
        )];
        let result = migrate(&tracks);
        let assigned = result.tracks[0].audio_input_connection_id.clone().unwrap();
        merge_generated_connections(&mut registry, result.generated_connections, &ports());

        let migrated = registry.get(&assigned).expect("id still resolves");
        assert_eq!(
            migrated.name, "Input 1 2",
            "the name is disambiguated but the id is untouched"
        );
    }

    /// A MIDI track that was never touched still holds `AllInputs` — its
    /// per-type default. Case A must recognise that as "unset" and take the
    /// legacy device, instead of treating it as an explicit conflicting choice.
    #[test]
    fn an_untouched_midi_track_takes_the_legacy_device_without_a_warning() {
        let tracks = vec![midi_track(
            "t1",
            LegacyTrackInputRouting::MidiDevice {
                device_id: "MPK Mini".into(),
            },
            TrackMidiInputRouting::AllInputs,
        )];
        let result = migrate(&tracks);

        assert_eq!(
            result.tracks[0].midi_input,
            TrackMidiInputRouting::MidiDevice {
                device_id: "MPK Mini".into()
            }
        );
        assert!(
            result.warnings.is_empty(),
            "AllInputs is the default for a MIDI track, so this is Case A not Case C"
        );
    }

    /// The same value on an *audio* track is non-default, so it is Case C.
    #[test]
    fn the_same_value_on_an_audio_track_is_a_conflict() {
        let tracks = vec![track(
            "t1",
            LegacyTrackInputRouting::MidiDevice {
                device_id: "MPK Mini".into(),
            },
            TrackMidiInputRouting::AllInputs,
        )];
        assert_eq!(migrate(&tracks).warnings.len(), 1);
    }
}
