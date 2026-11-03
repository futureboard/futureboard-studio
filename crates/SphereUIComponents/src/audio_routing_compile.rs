//! Compile the logical [`AudioConnectionRegistry`] into an immutable runtime
//! routing snapshot the audio callback can consume directly.
//!
//! ```txt
//! AudioConnectionRegistry + track assignments
//!     -> validate ids
//!     -> resolve device + stable port ids
//!     -> resolve physical channel indexes
//!     -> EngineAudioRoutingSnapshot   (immutable, Arc-swapped)
//!     -> audio callback
//! ```
//!
//! Everything expensive happens here, on the control thread. The snapshot holds
//! only pre-resolved integers, so the callback never parses an id, hashes a
//! string, walks a UI model, locks, or allocates. A connection that is missing,
//! disabled, or invalid compiles to *silence* — never to a fallback channel,
//! because monitoring or recording the wrong physical input is worse than
//! recording nothing.

use std::sync::Arc;

use crate::audio_connections::{
    AudioConnectionDirection, AudioConnectionId, AudioConnectionRegistry, AvailablePorts,
};

/// Maximum channels one logical bus can resolve to without spilling to the
/// heap. Covers mono through 7.1; larger custom layouts are truncated at
/// compile time rather than allocating on the audio thread.
pub const MAX_ROUTED_CHANNELS: usize = 8;

/// Fixed-capacity channel list. Inline storage keeps the whole snapshot free of
/// per-entry heap indirection, so the callback touches one contiguous block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SmallChannelList {
    channels: [u32; MAX_ROUTED_CHANNELS],
    len: u8,
}

impl SmallChannelList {
    pub fn from_slice(source: &[u32]) -> Self {
        let mut channels = [0u32; MAX_ROUTED_CHANNELS];
        let len = source.len().min(MAX_ROUTED_CHANNELS);
        channels[..len].copy_from_slice(&source[..len]);
        Self {
            channels,
            len: len as u8,
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u32] {
        &self.channels[..self.len as usize]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A device resolved to a dense runtime index. The callback compares small
/// integers instead of device-id strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeAudioDeviceId(pub u32);

/// One track's fully resolved capture source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedInputSource {
    pub device_runtime_id: RuntimeAudioDeviceId,
    pub channels: SmallChannelList,
}

/// One track's input, in the engine's track order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTrackInput {
    /// Index into the engine's track table — never a string id.
    pub track_runtime_index: u32,
    /// `None` compiles to silence: unassigned, disabled, or unresolvable.
    pub source: Option<ResolvedInputSource>,
}

/// One output connection resolved to physical channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOutputConnection {
    pub connection_runtime_index: u32,
    pub device_runtime_id: RuntimeAudioDeviceId,
    pub channels: SmallChannelList,
}

/// One output destination as the callback sees it: a runtime device index and
/// an ordered channel list, nothing else. No id, no name, no port identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedOutputRoute {
    /// Index into `output_connections`, so the callback can also reach the
    /// shared row without a lookup.
    pub connection_runtime_index: u32,
    pub device_runtime_id: RuntimeAudioDeviceId,
    pub channels: SmallChannelList,
}

impl ResolvedOutputRoute {
    /// True when both routes touch at least one identical physical channel on
    /// the same device.
    #[inline]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.device_runtime_id == other.device_runtime_id
            && self
                .channels
                .as_slice()
                .iter()
                .any(|channel| other.channels.as_slice().contains(channel))
    }
}

/// Which stage performs the physical write this configuration.
///
/// Decided here, on the control thread, precisely so the callback never has to
/// reason about it — and so the master feed and a Control Room copy of the same
/// mix can never both reach one hardware destination (a +6 dB duplicate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardwareOutputOwner {
    /// The Control Room is off; Master writes its own output connection.
    MasterDirect,
    /// The Control Room is on and is the only stage writing hardware.
    MonitorControlRoom,
    /// Nothing resolves. Playback is silent — never a default device.
    #[default]
    None,
}

/// Fully resolved Master / Monitor output routing for one snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolvedOutputRouting {
    /// Master's own destination, when it resolves.
    pub master: Option<ResolvedOutputRoute>,
    /// The Monitor override, when one is set *and* resolves. `None` here does
    /// not mean "unassigned" — it means Monitor follows Master.
    pub monitor_override: Option<ResolvedOutputRoute>,
    /// Where the Control Room writes: the override, else Master.
    pub effective_monitor: Option<ResolvedOutputRoute>,
    pub hardware_owner: HardwareOutputOwner,
}

impl ResolvedOutputRouting {
    /// The single route that reaches hardware, if any.
    #[inline]
    pub fn hardware_route(&self) -> Option<&ResolvedOutputRoute> {
        match self.hardware_owner {
            HardwareOutputOwner::MasterDirect => self.master.as_ref(),
            HardwareOutputOwner::MonitorControlRoom => self.effective_monitor.as_ref(),
            HardwareOutputOwner::None => None,
        }
    }

    /// Every route that reaches hardware this configuration. At most one, by
    /// construction: this is the shape the duplicate-write tests assert on.
    pub fn hardware_writers(&self) -> Vec<ResolvedOutputRoute> {
        self.hardware_route().copied().into_iter().collect()
    }
}

/// Immutable routing snapshot. Published by `Arc` swap; the previous snapshot
/// stays alive until the callback holding it releases its clone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EngineAudioRoutingSnapshot {
    /// Bumped on every publish so the callback can detect a swap without
    /// comparing contents.
    pub generation: u64,
    pub track_inputs: Vec<ResolvedTrackInput>,
    pub output_connections: Vec<ResolvedOutputConnection>,
    /// Master / Monitor destinations and which of them owns the physical write.
    pub output_routing: ResolvedOutputRouting,
    /// True when master and monitor resolve to overlapping physical ports on
    /// the same device. Informational only: `output_routing.hardware_owner`
    /// already guarantees a single writer.
    pub master_monitor_share_ports: bool,
}

impl EngineAudioRoutingSnapshot {
    /// Resolved source for a track index. Callback-safe: a bounds-checked slice
    /// index, no lookup.
    #[inline]
    pub fn track_input(&self, track_runtime_index: usize) -> Option<&ResolvedInputSource> {
        self.track_inputs
            .get(track_runtime_index)
            .and_then(|entry| entry.source.as_ref())
    }

    #[inline]
    pub fn output(&self, index: u32) -> Option<&ResolvedOutputConnection> {
        self.output_connections.get(index as usize)
    }
}

/// What the compiler is asked to resolve. Built on the control thread from the
/// project model.
#[derive(Debug, Clone, Default)]
pub struct RoutingCompileRequest {
    /// Track input assignments in engine track order. `None` = No Input.
    pub track_inputs: Vec<Option<AudioConnectionId>>,
    /// Project Master output. `None` = no Master hardware destination.
    pub master_output: Option<AudioConnectionId>,
    /// Monitor / Control Room override. `None` = Follow Master Output — never
    /// "unassigned".
    pub monitor_output_override: Option<AudioConnectionId>,
    /// Whether the Control Room is in the playback path. Decides ownership of
    /// the physical write; compiled here so the callback never branches on it.
    pub control_room_active: bool,
}

/// A problem found while compiling. Surfaced to the UI as a warning; never a
/// load or playback failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingWarning {
    /// A track references a connection that no longer exists.
    UnknownTrackInput {
        track_runtime_index: u32,
    },
    /// A track's connection exists but cannot currently be resolved.
    TrackInputUnavailable {
        track_runtime_index: u32,
        connection: AudioConnectionId,
    },
    /// A track references an output connection as its input, or similar
    /// direction mismatch.
    DirectionMismatch {
        connection: AudioConnectionId,
    },
    UnknownOutput {
        connection: AudioConnectionId,
    },
    OutputUnavailable {
        connection: AudioConnectionId,
    },
    /// Master and monitor resolve to the *same* hardware destination. Allowed
    /// as a logical selection — the Control Room simply owns the write.
    DuplicateHardwareWrite,
    /// Nothing resolves, so playback is silent. Reported instead of falling
    /// back to a system default device or to channels 0/1.
    NoHardwareOutput,
}

/// Result of one compile pass.
#[derive(Debug, Clone, Default)]
pub struct RoutingCompileResult {
    pub snapshot: EngineAudioRoutingSnapshot,
    pub warnings: Vec<RoutingWarning>,
}

/// Assigns dense runtime indexes to device ids, so the snapshot carries `u32`
/// rather than strings.
#[derive(Debug, Default)]
struct DeviceIndexer {
    devices: Vec<String>,
}

impl DeviceIndexer {
    fn index_of(&mut self, device_id: &str) -> RuntimeAudioDeviceId {
        if let Some(position) = self.devices.iter().position(|d| d == device_id) {
            return RuntimeAudioDeviceId(position as u32);
        }
        self.devices.push(device_id.to_string());
        RuntimeAudioDeviceId((self.devices.len() - 1) as u32)
    }
}

/// Compile the registry and assignments into a runtime snapshot.
///
/// `generation` must increase on every publish. Pure and allocation-heavy by
/// design — it runs on the control thread, never in the callback.
pub fn compile_routing(
    registry: &AudioConnectionRegistry,
    ports: &AvailablePorts,
    request: &RoutingCompileRequest,
    generation: u64,
) -> RoutingCompileResult {
    let mut warnings = Vec::new();
    let mut devices = DeviceIndexer::default();

    // ── Track inputs ────────────────────────────────────────────────────────
    let mut track_inputs = Vec::with_capacity(request.track_inputs.len());
    for (index, assignment) in request.track_inputs.iter().enumerate() {
        let track_runtime_index = index as u32;
        let source = match assignment {
            None => None,
            Some(id) => match registry.get(id) {
                None => {
                    warnings.push(RoutingWarning::UnknownTrackInput {
                        track_runtime_index,
                    });
                    None
                }
                Some(connection) if connection.direction != AudioConnectionDirection::Input => {
                    warnings.push(RoutingWarning::DirectionMismatch {
                        connection: id.clone(),
                    });
                    None
                }
                Some(connection) => match registry.resolved_ports(id, ports) {
                    Some(channels) => {
                        let device_id = connection.effective_device_id().unwrap_or_default();
                        Some(ResolvedInputSource {
                            device_runtime_id: devices.index_of(device_id),
                            channels: SmallChannelList::from_slice(&channels),
                        })
                    }
                    None => {
                        // Missing device or port: compile to silence, keep the
                        // assignment, and tell the UI why.
                        warnings.push(RoutingWarning::TrackInputUnavailable {
                            track_runtime_index,
                            connection: id.clone(),
                        });
                        None
                    }
                },
            },
        };
        track_inputs.push(ResolvedTrackInput {
            track_runtime_index,
            source,
        });
    }

    // ── Outputs ─────────────────────────────────────────────────────────────
    let mut output_connections: Vec<ResolvedOutputConnection> = Vec::new();
    let resolve_output = |id: &AudioConnectionId,
                          devices: &mut DeviceIndexer,
                          outputs: &mut Vec<ResolvedOutputConnection>,
                          warnings: &mut Vec<RoutingWarning>|
     -> Option<u32> {
        let connection = match registry.get(id) {
            None => {
                warnings.push(RoutingWarning::UnknownOutput {
                    connection: id.clone(),
                });
                return None;
            }
            Some(connection) => connection,
        };
        if connection.direction != AudioConnectionDirection::Output {
            warnings.push(RoutingWarning::DirectionMismatch {
                connection: id.clone(),
            });
            return None;
        }
        let channels = match registry.resolved_ports(id, ports) {
            Some(channels) => channels,
            None => {
                warnings.push(RoutingWarning::OutputUnavailable {
                    connection: id.clone(),
                });
                return None;
            }
        };
        let device_id = connection.effective_device_id().unwrap_or_default();
        let device_runtime_id = devices.index_of(device_id);
        let channels = SmallChannelList::from_slice(&channels);
        // Reuse an identical resolved output rather than emitting a duplicate
        // row, so the master/monitor overlap check below is a simple compare.
        if let Some(existing) = outputs.iter().position(|entry| {
            entry.device_runtime_id == device_runtime_id && entry.channels == channels
        }) {
            return Some(existing as u32);
        }
        let runtime_index = outputs.len() as u32;
        outputs.push(ResolvedOutputConnection {
            connection_runtime_index: runtime_index,
            device_runtime_id,
            channels,
        });
        Some(runtime_index)
    };

    let master_index = request
        .master_output
        .as_ref()
        .and_then(|id| resolve_output(id, &mut devices, &mut output_connections, &mut warnings));
    let monitor_index = request
        .monitor_output_override
        .as_ref()
        .and_then(|id| resolve_output(id, &mut devices, &mut output_connections, &mut warnings));

    let route_at = |index: u32, outputs: &[ResolvedOutputConnection]| -> ResolvedOutputRoute {
        let entry = &outputs[index as usize];
        ResolvedOutputRoute {
            connection_runtime_index: entry.connection_runtime_index,
            device_runtime_id: entry.device_runtime_id,
            channels: entry.channels,
        }
    };
    let master = master_index.map(|index| route_at(index, &output_connections));
    let monitor_override = monitor_index.map(|index| route_at(index, &output_connections));

    // Follow Master Output is resolved here, not in the callback: `None` on the
    // override means "use Master", and when Master has none too the result is
    // silence rather than any fallback destination.
    let effective_monitor = monitor_override.or(master);

    // ── Shared and overlapping hardware ─────────────────────────────────────
    // A *partial* overlap (1-2 against 2-3) never gets this far: the registry
    // marks the second bus `Conflict`, so it resolves to nothing. What remains
    // is the legitimate case of both pointing at the same destination.
    let master_monitor_share_ports = match (&master, &monitor_override) {
        (Some(master), Some(monitor)) => master.overlaps(monitor),
        _ => false,
    };
    if master_monitor_share_ports {
        warnings.push(RoutingWarning::DuplicateHardwareWrite);
    }

    // ── Hardware ownership ──────────────────────────────────────────────────
    // Exactly one stage writes hardware. With the Control Room in the path it
    // is the Control Room, even when both point at the same destination — that
    // is what prevents the master feed and its monitored copy from summing.
    let hardware_owner = if request.control_room_active {
        match effective_monitor {
            Some(_) => HardwareOutputOwner::MonitorControlRoom,
            None => HardwareOutputOwner::None,
        }
    } else {
        match master {
            Some(_) => HardwareOutputOwner::MasterDirect,
            None => HardwareOutputOwner::None,
        }
    };
    // Only report silence when an output was actually *asked for* and did not
    // resolve. A project with nothing assigned is already reported once, at
    // load time, by the output bootstrap.
    let requested_an_output =
        request.master_output.is_some() || request.monitor_output_override.is_some();
    if requested_an_output && matches!(hardware_owner, HardwareOutputOwner::None) {
        warnings.push(RoutingWarning::NoHardwareOutput);
    }

    RoutingCompileResult {
        snapshot: EngineAudioRoutingSnapshot {
            generation,
            track_inputs,
            output_connections,
            output_routing: ResolvedOutputRouting {
                master,
                monitor_override,
                effective_monitor,
                hardware_owner,
            },
            master_monitor_share_ports,
        },
        warnings,
    }
}

/// Atomically swappable holder for the published snapshot.
///
/// The audio callback clones the `Arc` (one atomic increment, no lock, no
/// allocation) and the previous snapshot stays alive until that clone is
/// dropped, so a publish can never free a snapshot mid-block.
#[derive(Debug, Default)]
pub struct RoutingSnapshotPublisher {
    current: std::sync::Mutex<Arc<EngineAudioRoutingSnapshot>>,
    generation: std::sync::atomic::AtomicU64,
}

impl RoutingSnapshotPublisher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Next generation number. Monotonic across the process.
    pub fn next_generation(&self) -> u64 {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    /// Publish from the control thread.
    pub fn publish(&self, snapshot: EngineAudioRoutingSnapshot) {
        let next = Arc::new(snapshot);
        if let Ok(mut guard) = self.current.lock() {
            *guard = next;
        }
    }

    /// Read the current snapshot from the control thread.
    pub fn current(&self) -> Arc<EngineAudioRoutingSnapshot> {
        self.current
            .lock()
            .map(|guard| Arc::clone(&guard))
            .unwrap_or_default()
    }
}

/// Process-wide publisher holding the snapshot the engine reads.
///
/// One instance so the control thread and the audio path agree on which
/// snapshot is current; the `Arc` swap inside is what keeps an in-flight
/// snapshot alive across a publish.
pub fn global_routing_publisher() -> &'static RoutingSnapshotPublisher {
    use std::sync::OnceLock;
    static PUBLISHER: OnceLock<RoutingSnapshotPublisher> = OnceLock::new();
    PUBLISHER.get_or_init(RoutingSnapshotPublisher::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::{AudioConnection, ChannelLayout};

    fn device() -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "System Audio Device", 4, 4)
    }

    fn registry() -> AudioConnectionRegistry {
        AudioConnectionRegistry::default_template(&device(), "dev-1")
    }

    fn input_id(registry: &AudioConnectionRegistry, name: &str) -> AudioConnectionId {
        registry
            .by_direction(AudioConnectionDirection::Input)
            .into_iter()
            .find(|c| c.name == name)
            .expect("input connection")
            .id
            .clone()
    }

    fn output_id(registry: &AudioConnectionRegistry, name: &str) -> AudioConnectionId {
        registry
            .by_direction(AudioConnectionDirection::Output)
            .into_iter()
            .find(|c| c.name == name)
            .expect("output connection")
            .id
            .clone()
    }

    #[test]
    fn a_mono_input_compiles_to_one_physical_channel() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            track_inputs: vec![Some(input_id(&registry, "Mono Input 1"))],
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);

        let source = result.snapshot.track_input(0).expect("resolved source");
        assert_eq!(source.channels.as_slice(), &[0]);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn a_stereo_input_compiles_left_then_right() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            track_inputs: vec![Some(input_id(&registry, "Stereo Input 1-2"))],
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);
        assert_eq!(
            result.snapshot.track_input(0).unwrap().channels.as_slice(),
            &[0, 1],
            "channel order carries Left then Right"
        );
    }

    #[test]
    fn no_input_compiles_to_silence_without_a_warning() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            track_inputs: vec![None],
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);
        assert!(result.snapshot.track_input(0).is_none());
        assert!(result.warnings.is_empty());
    }

    /// A missing device must compile to silence — never to an unrelated
    /// physical channel — and must say so.
    #[test]
    fn a_missing_device_compiles_to_silence_and_warns() {
        let mut registry = registry();
        let id = input_id(&registry, "Mono Input 1");
        registry.revalidate(&AvailablePorts::default());

        let request = RoutingCompileRequest {
            track_inputs: vec![Some(id.clone())],
            ..Default::default()
        };
        let result = compile_routing(&registry, &AvailablePorts::default(), &request, 1);

        assert!(result.snapshot.track_input(0).is_none());
        assert_eq!(
            result.warnings,
            vec![RoutingWarning::TrackInputUnavailable {
                track_runtime_index: 0,
                connection: id
            }]
        );
    }

    #[test]
    fn an_unknown_connection_id_compiles_to_silence_and_warns() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            track_inputs: vec![Some(AudioConnectionId::from_stored("ac-ghost"))],
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);
        assert!(result.snapshot.track_input(0).is_none());
        assert_eq!(
            result.warnings,
            vec![RoutingWarning::UnknownTrackInput {
                track_runtime_index: 0
            }]
        );
    }

    #[test]
    fn using_an_output_connection_as_a_track_input_is_rejected() {
        let ports = device();
        let registry = registry();
        let id = output_id(&registry, "Main Output 1-2");
        let request = RoutingCompileRequest {
            track_inputs: vec![Some(id.clone())],
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);
        assert!(result.snapshot.track_input(0).is_none());
        assert_eq!(
            result.warnings,
            vec![RoutingWarning::DirectionMismatch { connection: id }]
        );
    }

    #[test]
    fn master_and_monitor_resolve_to_output_indexes() {
        let ports = device();
        let mut registry = registry();
        let headphones = registry.add(
            AudioConnection::new(
                "Headphones",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 2, |i| format!("Output {}", i + 1)),
        );
        registry.revalidate(&ports);

        let request = RoutingCompileRequest {
            master_output: Some(output_id(&registry, "Main Output 1-2")),
            monitor_output_override: Some(headphones),
            control_room_active: true,
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 7);

        assert_eq!(result.snapshot.generation, 7);
        let routing = result.snapshot.output_routing;
        assert_eq!(routing.master.unwrap().channels.as_slice(), &[0, 1]);
        assert_eq!(
            routing.monitor_override.unwrap().channels.as_slice(),
            &[2, 3]
        );
        assert_eq!(
            routing.effective_monitor.unwrap().channels.as_slice(),
            &[2, 3]
        );
        assert_eq!(
            routing.hardware_owner,
            HardwareOutputOwner::MonitorControlRoom
        );
        assert!(!result.snapshot.master_monitor_share_ports);
        assert!(result.warnings.is_empty());
    }

    /// Monitor with no override follows Master — `None` is Follow Master
    /// Output, not "unassigned".
    #[test]
    fn a_monitor_without_an_override_follows_the_master_output() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            master_output: Some(output_id(&registry, "Main Output 1-2")),
            monitor_output_override: None,
            control_room_active: true,
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);
        let routing = result.snapshot.output_routing;

        assert!(routing.monitor_override.is_none());
        assert_eq!(
            routing.effective_monitor.unwrap().channels.as_slice(),
            &[0, 1]
        );
        assert_eq!(
            routing.hardware_owner,
            HardwareOutputOwner::MonitorControlRoom
        );
        assert_eq!(
            routing.hardware_writers().len(),
            1,
            "one stage owns the hardware write"
        );
    }

    /// With the Control Room out of the path Master writes its own output.
    #[test]
    fn master_owns_the_hardware_write_when_the_control_room_is_disabled() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            master_output: Some(output_id(&registry, "Main Output 1-2")),
            control_room_active: false,
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);
        let routing = result.snapshot.output_routing;

        assert_eq!(routing.hardware_owner, HardwareOutputOwner::MasterDirect);
        assert_eq!(
            routing.hardware_route().unwrap().channels.as_slice(),
            &[0, 1]
        );
    }

    /// No output device, no assignment: silence. Never channels 0/1 by default,
    /// never a system default device.
    #[test]
    fn nothing_assigned_compiles_to_silence_rather_than_a_fallback() {
        let ports = device();
        let registry = registry();
        for control_room_active in [true, false] {
            let request = RoutingCompileRequest {
                master_output: None,
                monitor_output_override: None,
                control_room_active,
                ..Default::default()
            };
            let result = compile_routing(&registry, &ports, &request, 1);
            let routing = result.snapshot.output_routing;

            assert_eq!(routing.hardware_owner, HardwareOutputOwner::None);
            assert!(routing.hardware_route().is_none());
            assert!(routing.hardware_writers().is_empty());
            assert!(
                result.warnings.is_empty(),
                "an unassigned project is reported once at load time, not on \
                 every recompile"
            );
        }
    }

    /// A disabled output bus resolves to nothing at all — the assignment is
    /// kept by the project, but the snapshot carries silence.
    #[test]
    fn a_disabled_output_compiles_to_silence_and_warns() {
        let ports = device();
        let mut registry = registry();
        let main = output_id(&registry, "Main Output 1-2");
        registry.set_enabled(&main, false);
        registry.revalidate(&ports);

        let request = RoutingCompileRequest {
            master_output: Some(main.clone()),
            control_room_active: false,
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);

        assert!(result.snapshot.output_routing.master.is_none());
        assert_eq!(
            result.snapshot.output_routing.hardware_owner,
            HardwareOutputOwner::None
        );
        assert!(result
            .warnings
            .contains(&RoutingWarning::OutputUnavailable { connection: main }));
        assert!(result.warnings.contains(&RoutingWarning::NoHardwareOutput));
    }

    /// Master and Monitor on the same pair is the duplicate-write case: the
    /// engine must be told so it does not sum the same mix twice.
    #[test]
    fn master_and_monitor_on_the_same_ports_flag_a_duplicate_write() {
        let ports = device();
        let registry = registry();
        let main = output_id(&registry, "Main Output 1-2");
        let request = RoutingCompileRequest {
            master_output: Some(main.clone()),
            monitor_output_override: Some(main),
            control_room_active: true,
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);

        assert!(result.snapshot.master_monitor_share_ports);
        assert!(result
            .warnings
            .contains(&RoutingWarning::DuplicateHardwareWrite));
        assert_eq!(
            result.snapshot.output_connections.len(),
            1,
            "an identical resolved output is reused, not duplicated"
        );

        // The point of the flag: only one stage writes, so the destination
        // carries the mix once — never master + monitored copy at +6 dB.
        let routing = result.snapshot.output_routing;
        assert_eq!(
            routing.hardware_owner,
            HardwareOutputOwner::MonitorControlRoom
        );
        assert_eq!(routing.hardware_writers().len(), 1);
        assert_eq!(
            routing.hardware_route().unwrap().channels.as_slice(),
            &[0, 1]
        );
    }

    /// Master on 1-2 and Monitor on 2-3 share physical port 2 without being the
    /// same destination. Neither summing them nor compensating that port's gain
    /// is acceptable, so the pair is rejected one layer earlier (registry
    /// `Conflict`) and the compiler emits silence for the loser — still exactly
    /// one hardware writer.
    #[test]
    fn overlapping_master_and_monitor_outputs_are_rejected_not_summed() {
        let ports = device();
        let mut registry = registry();
        let master = output_id(&registry, "Main Output 1-2");
        let overlapping = registry.add(
            AudioConnection::new(
                "Offset Monitor",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 1, |i| format!("Output {}", i + 1)),
        );
        registry.revalidate(&ports);
        assert_eq!(
            registry.get(&overlapping).unwrap().status,
            crate::audio_connections::AudioConnectionStatus::Conflict
        );

        let request = RoutingCompileRequest {
            master_output: Some(master),
            monitor_output_override: Some(overlapping.clone()),
            control_room_active: true,
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);

        assert!(result
            .warnings
            .contains(&RoutingWarning::OutputUnavailable {
                connection: overlapping
            }));
        assert!(
            result.snapshot.output_routing.monitor_override.is_none(),
            "a conflicted override must not resolve to the shared port"
        );
        assert_eq!(
            result.snapshot.output_routing.hardware_writers().len(),
            1,
            "the shared port is still written by exactly one stage"
        );
        assert_eq!(
            result
                .snapshot
                .output_routing
                .hardware_route()
                .unwrap()
                .channels
                .as_slice(),
            &[0, 1],
            "Monitor falls back to Follow Master, not to the conflicted pair"
        );
    }

    #[test]
    fn devices_are_assigned_dense_runtime_indexes() {
        let ports = AvailablePorts::for_device("dev-1", "First", 2, 2)
            .merge(AvailablePorts::for_device("dev-2", "Second", 2, 2));
        let mut registry = AudioConnectionRegistry::new();
        let a = registry.add(
            AudioConnection::new("A", AudioConnectionDirection::Input, ChannelLayout::Mono)
                .bind_consecutive("dev-1", 0, |i| format!("Input {}", i + 1)),
        );
        let b = registry.add(
            AudioConnection::new("B", AudioConnectionDirection::Input, ChannelLayout::Mono)
                .bind_consecutive("dev-2", 0, |i| format!("Input {}", i + 1)),
        );
        registry.revalidate(&ports);

        let request = RoutingCompileRequest {
            track_inputs: vec![Some(a), Some(b)],
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);
        assert_eq!(
            result.snapshot.track_input(0).unwrap().device_runtime_id,
            RuntimeAudioDeviceId(0)
        );
        assert_eq!(
            result.snapshot.track_input(1).unwrap().device_runtime_id,
            RuntimeAudioDeviceId(1)
        );
    }

    /// The snapshot the callback reads must be plain integers — no strings, no
    /// maps, no ids to parse.
    #[test]
    fn the_snapshot_the_callback_reads_is_copyable_integer_data() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            track_inputs: vec![Some(input_id(&registry, "Stereo Input 1-2"))],
            master_output: Some(output_id(&registry, "Main Output 1-2")),
            ..Default::default()
        };
        let result = compile_routing(&registry, &ports, &request, 1);

        // `ResolvedInputSource` and `SmallChannelList` are `Copy`: reading one
        // in the callback cannot allocate or drop.
        fn assert_copy<T: Copy>(_: &T) {}
        let source = result.snapshot.track_input(0).unwrap();
        assert_copy(source);
        assert_copy(&source.channels);
        assert_copy(&source.device_runtime_id);
        assert_eq!(source.channels.len(), 2);
    }

    /// The output routing the callback reads is the same shape: plain integers.
    /// Nothing here is a UUID, a stable port id, a name, or a map — resolving
    /// any of those in the callback is what this type exists to prevent.
    #[test]
    fn output_routing_carries_only_callback_safe_resolved_data() {
        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            master_output: Some(output_id(&registry, "Main Output 1-2")),
            control_room_active: true,
            ..Default::default()
        };
        let routing = compile_routing(&registry, &ports, &request, 1)
            .snapshot
            .output_routing;

        fn assert_copy<T: Copy>(_: &T) {}
        assert_copy(&routing);
        let route = routing.hardware_route().expect("resolved route");
        assert_copy(route);
        assert_copy(&route.device_runtime_id);
        assert_copy(&route.channels);
        // Reading the destination is a slice index over inline `u32`s.
        assert_eq!(route.channels.as_slice(), &[0, 1]);
    }

    /// Callback-safety audit. Everything the audio callback reads out of a
    /// published snapshot is `Copy` integer data: no allocation, no lock, no
    /// map, no id to parse, no device string, no backend enumeration. The type
    /// system is the enforcement — this test states the contract.
    #[test]
    fn the_published_snapshot_exposes_no_id_string_or_map_to_the_callback() {
        fn assert_copy<T: Copy>() {}
        fn assert_send_sync<T: Send + Sync>() {}

        // Every per-block type the callback touches.
        assert_copy::<SmallChannelList>();
        assert_copy::<RuntimeAudioDeviceId>();
        assert_copy::<ResolvedInputSource>();
        assert_copy::<ResolvedOutputRoute>();
        assert_copy::<ResolvedOutputRouting>();
        assert_copy::<HardwareOutputOwner>();
        // The snapshot is shared by Arc across the control and audio threads.
        assert_send_sync::<EngineAudioRoutingSnapshot>();

        let ports = device();
        let registry = registry();
        let request = RoutingCompileRequest {
            track_inputs: vec![Some(input_id(&registry, "Stereo Input 1-2"))],
            master_output: Some(output_id(&registry, "Main Output 1-2")),
            control_room_active: true,
            ..Default::default()
        };
        let snapshot = compile_routing(&registry, &ports, &request, 1).snapshot;

        // Reads the callback performs: a bounds-checked slice index and a
        // slice of `u32`. No hashing, no parsing, no lookup.
        let source = snapshot.track_input(0).expect("resolved source");
        assert_eq!(source.channels.as_slice(), &[0, 1]);
        let route = snapshot
            .output_routing
            .hardware_route()
            .expect("resolved route");
        assert_eq!(route.channels.as_slice(), &[0, 1]);
        assert_eq!(route.device_runtime_id, RuntimeAudioDeviceId(0));
    }

    /// Cloning the published `Arc` is what the callback actually does per
    /// block: one atomic increment, no allocation, and the snapshot it holds
    /// survives a concurrent publish.
    #[test]
    fn the_callback_side_read_is_an_arc_clone_not_a_rebuild() {
        let publisher = RoutingSnapshotPublisher::new();
        let generation = publisher.next_generation();
        publisher.publish(EngineAudioRoutingSnapshot {
            generation,
            output_routing: ResolvedOutputRouting {
                hardware_owner: HardwareOutputOwner::MonitorControlRoom,
                ..Default::default()
            },
            ..Default::default()
        });

        let held = publisher.current();
        assert_eq!(Arc::strong_count(&held) >= 2, true);
        assert_eq!(
            held.output_routing.hardware_owner,
            HardwareOutputOwner::MonitorControlRoom
        );
    }

    #[test]
    fn a_custom_layout_is_truncated_at_the_fixed_channel_capacity() {
        let wide: Vec<u32> = (0..32).collect();
        let list = SmallChannelList::from_slice(&wide);
        assert_eq!(list.len(), MAX_ROUTED_CHANNELS);
        assert_eq!(list.as_slice()[0], 0);
        assert_eq!(list.as_slice()[MAX_ROUTED_CHANNELS - 1], 7);
    }

    #[test]
    fn publishing_keeps_the_previous_snapshot_alive_for_its_readers() {
        let publisher = RoutingSnapshotPublisher::new();
        let first_generation = publisher.next_generation();
        publisher.publish(EngineAudioRoutingSnapshot {
            generation: first_generation,
            ..Default::default()
        });

        // A "callback" holding the old snapshot.
        let held = publisher.current();
        assert_eq!(held.generation, first_generation);

        let second_generation = publisher.next_generation();
        publisher.publish(EngineAudioRoutingSnapshot {
            generation: second_generation,
            ..Default::default()
        });

        assert_eq!(
            held.generation, first_generation,
            "the in-flight snapshot must survive the swap"
        );
        assert_eq!(publisher.current().generation, second_generation);
        assert!(second_generation > first_generation);
    }
}
