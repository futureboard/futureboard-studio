//! Audio Connections — the single source of truth for logical audio buses.
//!
//! Every part of Studio that needs an audio input or output selects a *logical
//! connection* from this registry. Track inspectors, the Add Track dialog, the
//! Master bus, and the Control Room all reference an [`AudioConnectionId`];
//! none of them may reach for a raw hardware port. This layer is the only place
//! where a logical bus is mapped to physical ports.
//!
//! ```txt
//! Physical device ports
//!     -> AudioConnection (logical bus, stable id)
//!         -> Track input / Master output / Monitor output
//! ```
//!
//! Two invariants make the indirection worth it:
//!
//! * **Stable identity.** Ids are UUID-shaped and independent of the display
//!   name, so renaming a bus can never break a track's routing.
//! * **Non-destructive device loss.** When a device disappears the connection
//!   and every reference to it survive; only [`AudioConnectionStatus`] changes.
//!   Reconnecting the same device restores the mapping exactly, because
//!   bindings are keyed by a stable port identifier rather than by index.
//!
//! A bus whose device is missing still runs on the device the engine uses for
//! its direction (the Audio Device Setup pick, or the system default): it
//! reaches the same port numbers there ([`AudioConnectionStatus::StandIn`]). A project saved on one backend
//! (an ASIO driver) therefore records and plays on another (WASAPI) without
//! re-patching, and nothing stored changes, so the original device takes over
//! again as soon as it is back.

use std::collections::{HashMap, HashSet};

/// Stable identity for a logical audio bus.
///
/// Deliberately opaque and *not* derived from the bus name, so renaming is a
/// pure display change. Stored in projects and referenced by tracks and buses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AudioConnectionId(String);

impl AudioConnectionId {
    /// Mint a new unique id. Not a real UUID crate dependency — a
    /// process-unique, time-seeded value is enough for project-local identity
    /// and keeps this crate dependency-free.
    pub fn generate() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self(format!("ac-{nanos:016x}-{seq:08x}"))
    }

    /// Rebuild from a persisted string.
    pub fn from_stored(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Direction of a logical bus. An input connection may only bind capture
/// ports; an output connection may only bind playback ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioConnectionDirection {
    Input,
    Output,
}

impl AudioConnectionDirection {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "input" => Some(Self::Input),
            "output" => Some(Self::Output),
            _ => None,
        }
    }
}

/// Channel layout of a logical bus.
///
/// `Custom` exists so surround and other multichannel layouts can be added
/// without changing the persisted shape or the binding model — every layout is
/// just a channel count with per-channel bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLayout {
    Mono,
    Stereo,
    Custom { channels: usize },
}

impl ChannelLayout {
    pub fn channel_count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Custom { channels } => channels.max(1),
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::Custom { .. } => "custom",
        }
    }

    /// Conventional name for one logical channel, used by the per-channel port
    /// selectors. A stereo pair is always two separately addressable channels —
    /// never one ambiguous "1/2" string internally.
    pub fn channel_label(self, logical_channel: usize) -> String {
        match (self, logical_channel) {
            (Self::Mono, _) => "Mono".to_string(),
            (Self::Stereo, 0) => "Left".to_string(),
            (Self::Stereo, 1) => "Right".to_string(),
            _ => format!("Ch {}", logical_channel + 1),
        }
    }

    pub fn from_parts(tag: &str, channels: usize) -> Self {
        match tag {
            "mono" => Self::Mono,
            "stereo" => Self::Stereo,
            _ => Self::Custom { channels },
        }
    }
}

/// Stable identifier for one physical port on one device.
///
/// Carries the device-scoped port *name* alongside its index. Restoration
/// prefers the name, so a driver that re-orders its ports between sessions
/// cannot silently move a bus onto an unrelated input.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AudioPortId {
    pub device_id: String,
    /// Driver-reported port name, e.g. `"Input 1"`. Empty when the backend does
    /// not name ports, in which case the index is the only key available.
    pub port_name: String,
    /// 0-based channel index on the device.
    pub port_index: u32,
}

impl AudioPortId {
    pub fn new(
        device_id: impl Into<String>,
        port_name: impl Into<String>,
        port_index: u32,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            port_name: port_name.into(),
            port_index,
        }
    }
}

/// One logical channel bound to one physical port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPortBinding {
    pub logical_channel: usize,
    pub physical_port_id: AudioPortId,
}

/// Health of a connection against the currently available hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioConnectionStatus {
    Active,
    /// The configured device is not present. The connection and every
    /// reference to it are preserved; it restores when the device returns.
    DeviceMissing,
    /// The configured device is not present, so the bus runs on the device
    /// selected in Audio Device Setup, at the same port numbers. Usable;
    /// nothing stored changes.
    StandIn,
    /// The device is present but a bound port is not.
    PortMissing,
    /// No ports are bound yet.
    Disconnected,
    Disabled,
    /// Two logical channels of this bus resolve to the same physical port, or
    /// (for outputs) another bus already writes the same port.
    Conflict,
}

impl AudioConnectionStatus {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Active | Self::StandIn)
    }

    /// Short label for the Status column.
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::DeviceMissing => "Device Missing",
            Self::StandIn => "Current Device",
            Self::PortMissing => "Port Missing",
            Self::Disconnected => "Disconnected",
            Self::Disabled => "Disabled",
            Self::Conflict => "Conflict",
        }
    }
}

/// One logical audio bus.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioConnection {
    pub id: AudioConnectionId,
    pub name: String,
    pub direction: AudioConnectionDirection,
    pub channel_layout: ChannelLayout,
    pub device_id: Option<String>,
    pub port_bindings: Vec<AudioPortBinding>,
    pub enabled: bool,
    /// Recomputed by [`AudioConnectionRegistry::revalidate`]; never persisted,
    /// because it describes the current machine rather than the project.
    pub status: AudioConnectionStatus,
    /// The device this bus runs on while [`AudioConnectionStatus::StandIn`].
    /// Recomputed with `status`; never persisted.
    pub stand_in_device: Option<String>,
}

impl AudioConnection {
    pub fn new(
        name: impl Into<String>,
        direction: AudioConnectionDirection,
        channel_layout: ChannelLayout,
    ) -> Self {
        Self {
            id: AudioConnectionId::generate(),
            name: name.into(),
            direction,
            channel_layout,
            device_id: None,
            port_bindings: Vec::new(),
            enabled: true,
            status: AudioConnectionStatus::Disconnected,
            stand_in_device: None,
        }
    }

    /// The device this bus reaches right now: the stand-in while its own
    /// device is missing, otherwise its own.
    pub fn effective_device_id(&self) -> Option<&str> {
        self.stand_in_device
            .as_deref()
            .or(self.device_id.as_deref())
    }

    /// Bind consecutive device ports starting at `first_port`, one per logical
    /// channel. The common case for "Stereo Input 1-2" style buses.
    pub fn bind_consecutive(
        mut self,
        device_id: impl Into<String>,
        first_port: u32,
        port_name: impl Fn(u32) -> String,
    ) -> Self {
        let device_id = device_id.into();
        let channels = self.channel_layout.channel_count();
        self.port_bindings = (0..channels)
            .map(|channel| {
                let index = first_port + channel as u32;
                AudioPortBinding {
                    logical_channel: channel,
                    physical_port_id: AudioPortId::new(&device_id, port_name(index), index),
                }
            })
            .collect();
        self.device_id = Some(device_id);
        self
    }

    /// Port bound to `logical_channel`, if any.
    pub fn binding(&self, logical_channel: usize) -> Option<&AudioPortBinding> {
        self.port_bindings
            .iter()
            .find(|binding| binding.logical_channel == logical_channel)
    }

    /// Human-readable port summary for the Device Port(s) column, e.g.
    /// `"Input 1 / Input 2"`. Display only — the model always keeps the
    /// per-channel bindings separate.
    pub fn port_summary(&self) -> String {
        if self.port_bindings.is_empty() {
            return "—".to_string();
        }
        let mut ordered: Vec<_> = self.port_bindings.iter().collect();
        ordered.sort_by_key(|binding| binding.logical_channel);
        ordered
            .iter()
            .map(|binding| {
                let port = &binding.physical_port_id;
                if port.port_name.is_empty() {
                    format!("{}", port.port_index + 1)
                } else {
                    port.port_name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// Resize the bindings to match a new layout, keeping the channels that
    /// still exist. Growing leaves the new channels unbound rather than
    /// guessing a port.
    pub fn set_channel_layout(&mut self, layout: ChannelLayout) {
        self.channel_layout = layout;
        let channels = layout.channel_count();
        self.port_bindings
            .retain(|binding| binding.logical_channel < channels);
    }
}

/// One physical port the current hardware actually exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailablePort {
    pub device_id: String,
    pub device_name: String,
    pub port_name: String,
    pub port_index: u32,
    pub direction: AudioConnectionDirection,
}

/// Snapshot of the ports currently available, used to validate and restore
/// connections. Built from the device registry; never stored in a project.
#[derive(Debug, Clone, Default)]
pub struct AvailablePorts {
    pub ports: Vec<AvailablePort>,
    /// The devices the engine opens (the Audio Device Setup pick, else the
    /// system default), by id. A bus whose own device is missing runs on
    /// these; see [`AudioConnectionStatus::StandIn`].
    pub stand_in_input: Option<String>,
    pub stand_in_output: Option<String>,
}

impl AvailablePorts {
    /// Build a device's ports from its channel counts.
    pub fn for_device(
        device_id: impl Into<String>,
        device_name: impl Into<String>,
        input_channels: u32,
        output_channels: u32,
    ) -> Self {
        let device_id = device_id.into();
        let device_name = device_name.into();
        let mut ports = Vec::new();
        for index in 0..input_channels {
            ports.push(AvailablePort {
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                port_name: format!("Input {}", index + 1),
                port_index: index,
                direction: AudioConnectionDirection::Input,
            });
        }
        for index in 0..output_channels {
            ports.push(AvailablePort {
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                port_name: format!("Output {}", index + 1),
                port_index: index,
                direction: AudioConnectionDirection::Output,
            });
        }
        Self {
            ports,
            ..Self::default()
        }
    }

    pub fn merge(mut self, other: AvailablePorts) -> Self {
        self.ports.extend(other.ports);
        self.stand_in_input = self.stand_in_input.or(other.stand_in_input);
        self.stand_in_output = self.stand_in_output.or(other.stand_in_output);
        self
    }

    /// Take the devices selected in Audio Device Setup (ids or display names)
    /// as stand-ins. A selection that is not among these ports is dropped:
    /// a stand-in has to exist.
    pub fn with_stand_ins(mut self, input: &str, output: &str) -> Self {
        let pick = |ports: &Self, selected: &str, direction| {
            let selected = selected.trim();
            (!selected.is_empty())
                .then(|| ports.matching_device_id(selected, direction))
                .flatten()
                .map(str::to_string)
        };
        self.stand_in_input = pick(&self, input, AudioConnectionDirection::Input);
        self.stand_in_output = pick(&self, output, AudioConnectionDirection::Output);
        self
    }

    pub fn stand_in(&self, direction: AudioConnectionDirection) -> Option<&str> {
        match direction {
            AudioConnectionDirection::Input => self.stand_in_input.as_deref(),
            AudioConnectionDirection::Output => self.stand_in_output.as_deref(),
        }
    }

    /// Resolve one binding: on the stand-in by port number when one is given
    /// (its port names belong to another device), otherwise as [`Self::resolve`].
    fn resolve_binding(
        &self,
        binding: &AudioPortBinding,
        direction: AudioConnectionDirection,
        stand_in: Option<&str>,
    ) -> Option<&AvailablePort> {
        match stand_in {
            Some(device_id) => self
                .ports_for(device_id, direction)
                .into_iter()
                .find(|port| port.port_index == binding.physical_port_id.port_index),
            None => self.resolve(&binding.physical_port_id, direction),
        }
    }

    pub fn has_device(&self, device_id: &str) -> bool {
        self.ports.iter().any(|port| port.device_id == device_id)
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.ports
            .iter()
            .find(|port| port.device_id == device_id)
            .map(|port| port.device_name.as_str())
    }

    /// Resolve a persisted device selection, which is historically a display
    /// name, to the stable id used by Audio Connections. Backend-specific
    /// devices such as ASIO expose a registry id that intentionally differs
    /// from the name shown in Audio Device Setup.
    pub fn matching_device_id(
        &self,
        selected: &str,
        direction: AudioConnectionDirection,
    ) -> Option<&str> {
        self.ports
            .iter()
            .find(|port| {
                port.direction == direction
                    && (port.device_id == selected || port.device_name == selected)
            })
            .map(|port| port.device_id.as_str())
    }

    pub fn ports_for(
        &self,
        device_id: &str,
        direction: AudioConnectionDirection,
    ) -> Vec<&AvailablePort> {
        self.ports
            .iter()
            .filter(|port| port.device_id == device_id && port.direction == direction)
            .collect()
    }

    /// Resolve a stored binding against current hardware.
    ///
    /// Name first, index second: a driver that re-orders ports between sessions
    /// must not silently move a bus onto an unrelated port. When the name no
    /// longer exists the index is accepted only if nothing else claimed that
    /// name, which is what makes reconnecting the same device restore exactly.
    pub fn resolve(
        &self,
        port: &AudioPortId,
        direction: AudioConnectionDirection,
    ) -> Option<&AvailablePort> {
        let candidates = self.ports_for(&port.device_id, direction);
        if !port.port_name.is_empty() {
            if let Some(found) = candidates
                .iter()
                .find(|candidate| candidate.port_name == port.port_name)
            {
                return Some(found);
            }
        }
        candidates
            .into_iter()
            .find(|candidate| candidate.port_index == port.port_index)
    }
}

/// What a removal would break, so the UI can name it before confirming.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionUsage {
    /// Ids of tracks whose input references the connection.
    pub track_ids: Vec<String>,
    /// Labels of non-track referents ("Master Bus", "Monitor Bus").
    pub buses: Vec<String>,
}

impl ConnectionUsage {
    pub fn is_empty(&self) -> bool {
        self.track_ids.is_empty() && self.buses.is_empty()
    }
}

/// One place in the project that points at an Audio Connection.
///
/// Every referent is enumerated here so a removal can describe *all* of its
/// consequences before it happens — a bus used only by Master must not look
/// unused just because no track input references it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioConnectionReference {
    /// A track's audio input, by track id.
    TrackInput(String),
    /// The project's Master output.
    MasterOutput,
    /// The Monitor / Control Room output override.
    MonitorOutput,
}

impl AudioConnectionReference {
    /// Sentence naming the referent, used in the removal confirmation.
    /// `track_name` resolves a [`Self::TrackInput`]; the bus variants ignore it.
    pub fn describe(&self, track_name: Option<&str>) -> String {
        match self {
            Self::TrackInput(track_id) => {
                format!("{} (track input)", track_name.unwrap_or(track_id))
            }
            Self::MasterOutput => "Master".to_string(),
            Self::MonitorOutput => "Monitor".to_string(),
        }
    }
}

/// The project's Audio Connections. Owns every logical bus and is the only
/// layer that knows about physical ports.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AudioConnectionRegistry {
    connections: Vec<AudioConnection>,
}

impl AudioConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the application-level default template from the active device.
    ///
    /// Only creates rows for ports that actually exist, so a stereo interface
    /// does not get phantom "Mono Input 3" entries. Callers persist the result
    /// with the project; this is never re-run over an existing registry, so a
    /// deleted default stays deleted.
    pub fn default_template(ports: &AvailablePorts, device_id: &str) -> Self {
        let mut registry = Self::new();
        let inputs = ports
            .ports_for(device_id, AudioConnectionDirection::Input)
            .len() as u32;
        let outputs = ports
            .ports_for(device_id, AudioConnectionDirection::Output)
            .len() as u32;

        for index in 0..inputs.min(2) {
            let connection = AudioConnection::new(
                format!("Mono Input {}", index + 1),
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
            )
            .bind_consecutive(device_id, index, |i| format!("Input {}", i + 1));
            registry.connections.push(connection);
        }
        if inputs >= 2 {
            let connection = AudioConnection::new(
                "Stereo Input 1-2",
                AudioConnectionDirection::Input,
                ChannelLayout::Stereo,
            )
            .bind_consecutive(device_id, 0, |i| format!("Input {}", i + 1));
            registry.connections.push(connection);
        }
        if outputs >= 2 {
            let connection = AudioConnection::new(
                "Main Output 1-2",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive(device_id, 0, |i| format!("Output {}", i + 1));
            registry.connections.push(connection);
        }
        registry.revalidate(ports);
        registry
    }

    /// Copy the application-level **Default Audio Connections** template into a
    /// fresh project registry.
    ///
    /// Every row gets a new project-local id, so the project is never linked to
    /// the template afterwards: editing the template later cannot reach an
    /// existing project, and a project edit cannot reach the template. Rows
    /// whose ports the current hardware does not expose are skipped rather than
    /// created as phantoms that would only ever read Port Missing.
    pub fn from_default_template(
        template: &crate::settings::DefaultAudioConnectionsSettings,
        ports: &AvailablePorts,
        input_device_id: &str,
        output_device_id: &str,
    ) -> Self {
        let mut registry = Self::new();
        let copy = |rows: &[crate::settings::DefaultAudioConnection],
                    direction: AudioConnectionDirection,
                    selected_device: &str,
                    registry: &mut Self| {
            if selected_device.trim().is_empty() {
                return;
            }
            let Some(device_id) = ports.matching_device_id(selected_device, direction) else {
                return;
            };
            let available = ports.ports_for(device_id, direction).len() as u32;
            let port_name = |index: u32| match direction {
                AudioConnectionDirection::Input => format!("Input {}", index + 1),
                AudioConnectionDirection::Output => format!("Output {}", index + 1),
            };
            for row in rows {
                let layout = ChannelLayout::from_parts(&row.channel_layout, 2);
                let needed = layout.channel_count() as u32;
                if row.first_port_index + needed > available {
                    continue;
                }
                let mut connection = AudioConnection::new(row.name.clone(), direction, layout)
                    .bind_consecutive(device_id, row.first_port_index, port_name);
                connection.enabled = row.enabled;
                registry.add(connection);
            }
        };
        copy(
            &template.inputs,
            AudioConnectionDirection::Input,
            input_device_id,
            &mut registry,
        );
        copy(
            &template.outputs,
            AudioConnectionDirection::Output,
            output_device_id,
            &mut registry,
        );
        registry.revalidate(ports);
        registry
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn all(&self) -> &[AudioConnection] {
        &self.connections
    }

    /// Connections in one direction, in table order.
    pub fn by_direction(&self, direction: AudioConnectionDirection) -> Vec<&AudioConnection> {
        self.connections
            .iter()
            .filter(|connection| connection.direction == direction)
            .collect()
    }

    pub fn get(&self, id: &AudioConnectionId) -> Option<&AudioConnection> {
        self.connections.iter().find(|c| c.id == *id)
    }

    pub fn get_mut(&mut self, id: &AudioConnectionId) -> Option<&mut AudioConnection> {
        self.connections.iter_mut().find(|c| c.id == *id)
    }

    /// Display name for a referenced connection, or `None` when the reference
    /// no longer resolves.
    pub fn name_of(&self, id: &AudioConnectionId) -> Option<&str> {
        self.get(id).map(|c| c.name.as_str())
    }

    /// Add a connection, disambiguating its name if needed. Returns its id.
    pub fn add(&mut self, mut connection: AudioConnection) -> AudioConnectionId {
        connection.name = self.unique_name(&connection.name, None);
        let id = connection.id.clone();
        self.connections.push(connection);
        id
    }

    /// Remove a connection. Returns it so the caller can undo or report.
    pub fn remove(&mut self, id: &AudioConnectionId) -> Option<AudioConnection> {
        let index = self.connections.iter().position(|c| c.id == *id)?;
        Some(self.connections.remove(index))
    }

    /// Rename a connection. The id is unchanged, so every track and bus
    /// reference keeps working.
    pub fn rename(&mut self, id: &AudioConnectionId, name: impl Into<String>) -> bool {
        let name = name.into();
        let unique = self.unique_name(&name, Some(id));
        match self.get_mut(id) {
            Some(connection) => {
                connection.name = unique;
                true
            }
            None => false,
        }
    }

    /// Duplicate a connection, giving the copy a fresh id and a distinct name.
    pub fn duplicate(&mut self, id: &AudioConnectionId) -> Option<AudioConnectionId> {
        let source = self.get(id)?.clone();
        let mut copy = source;
        copy.id = AudioConnectionId::generate();
        copy.name = self.unique_name(&format!("{} Copy", copy.name), None);
        let new_id = copy.id.clone();
        self.connections.push(copy);
        Some(new_id)
    }

    /// Replace the whole set (project load).
    pub fn replace_all(&mut self, connections: Vec<AudioConnection>) {
        self.connections = connections;
    }

    /// Reset to the default template for `device_id`, discarding user buses.
    pub fn reset_to_defaults(&mut self, ports: &AvailablePorts, device_id: &str) {
        *self = Self::default_template(ports, device_id);
    }

    /// A name not already taken by another connection, appending ` 2`, ` 3`, …
    /// Duplicate names are disambiguated rather than rejected so a rename never
    /// fails outright.
    fn unique_name(&self, desired: &str, skip: Option<&AudioConnectionId>) -> String {
        let taken: HashSet<&str> = self
            .connections
            .iter()
            .filter(|c| skip != Some(&c.id))
            .map(|c| c.name.as_str())
            .collect();
        if !taken.contains(desired) {
            return desired.to_string();
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{desired} {suffix}");
            if !taken.contains(candidate.as_str()) {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// Bind one logical channel to a physical port.
    ///
    /// Rejects a port whose direction does not match the bus: an input bus can
    /// never claim an output port and vice versa.
    pub fn bind_port(
        &mut self,
        id: &AudioConnectionId,
        logical_channel: usize,
        port: AudioPortId,
        port_direction: AudioConnectionDirection,
    ) -> bool {
        let Some(connection) = self.get_mut(id) else {
            return false;
        };
        if connection.direction != port_direction {
            return false;
        }
        if logical_channel >= connection.channel_layout.channel_count() {
            return false;
        }
        connection.device_id = Some(port.device_id.clone());
        match connection
            .port_bindings
            .iter_mut()
            .find(|binding| binding.logical_channel == logical_channel)
        {
            Some(binding) => binding.physical_port_id = port,
            None => connection.port_bindings.push(AudioPortBinding {
                logical_channel,
                physical_port_id: port,
            }),
        }
        true
    }

    /// Clear one logical channel's binding.
    pub fn clear_binding(&mut self, id: &AudioConnectionId, logical_channel: usize) -> bool {
        let Some(connection) = self.get_mut(id) else {
            return false;
        };
        let before = connection.port_bindings.len();
        connection
            .port_bindings
            .retain(|binding| binding.logical_channel != logical_channel);
        before != connection.port_bindings.len()
    }

    /// Point a connection at a different device, rebinding each logical channel
    /// to the same-numbered port on the new device where one exists.
    pub fn set_device(
        &mut self,
        id: &AudioConnectionId,
        device_id: &str,
        ports: &AvailablePorts,
    ) -> bool {
        let Some(connection) = self.get_mut(id) else {
            return false;
        };
        let direction = connection.direction;
        let available = ports.ports_for(device_id, direction);
        connection.device_id = Some(device_id.to_string());
        connection.port_bindings.retain(|binding| {
            available
                .iter()
                .any(|port| port.port_index == binding.physical_port_id.port_index)
        });
        for binding in connection.port_bindings.iter_mut() {
            if let Some(port) = available
                .iter()
                .find(|port| port.port_index == binding.physical_port_id.port_index)
            {
                binding.physical_port_id =
                    AudioPortId::new(device_id, port.port_name.clone(), port.port_index);
            }
        }
        true
    }

    /// Change a connection's channel layout, preserving surviving bindings.
    pub fn set_channel_layout(&mut self, id: &AudioConnectionId, layout: ChannelLayout) -> bool {
        match self.get_mut(id) {
            Some(connection) => {
                connection.set_channel_layout(layout);
                true
            }
            None => false,
        }
    }

    pub fn set_enabled(&mut self, id: &AudioConnectionId, enabled: bool) -> bool {
        match self.get_mut(id) {
            Some(connection) => {
                connection.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Recompute every connection's status against the available hardware, and
    /// restore bindings whose ports came back.
    ///
    /// A connection is never mutated destructively here: a missing device
    /// leaves the bindings untouched so that reconnecting the same device
    /// restores the mapping exactly.
    pub fn revalidate(&mut self, ports: &AvailablePorts) {
        // Output ports already claimed, so a second bus writing the same port
        // is reported as a Conflict rather than silently doubling the signal.
        let mut claimed_output_ports: HashMap<(String, u32), usize> = HashMap::new();

        for index in 0..self.connections.len() {
            let (status, stand_in) = {
                let connection = &self.connections[index];
                Self::status_for(connection, ports, &claimed_output_ports)
            };
            let direction = self.connections[index].direction;
            // Refresh port names for still-present ports so a driver rename
            // shows through without changing which port is bound. A stand-in
            // leaves the stored bindings alone: they belong to the missing
            // device.
            if matches!(status, AudioConnectionStatus::Active) {
                for binding in self.connections[index].port_bindings.iter_mut() {
                    if let Some(port) = ports.resolve(&binding.physical_port_id, direction) {
                        binding.physical_port_id.port_name = port.port_name.clone();
                        binding.physical_port_id.port_index = port.port_index;
                    }
                }
            }
            if status.is_usable() && direction == AudioConnectionDirection::Output {
                for binding in &self.connections[index].port_bindings {
                    let device_id = stand_in
                        .clone()
                        .unwrap_or_else(|| binding.physical_port_id.device_id.clone());
                    claimed_output_ports
                        .insert((device_id, binding.physical_port_id.port_index), index);
                }
            }
            self.connections[index].status = status;
            self.connections[index].stand_in_device = stand_in;
        }
    }

    /// Status against `ports`, and the stand-in device when the bus runs on
    /// one.
    fn status_for(
        connection: &AudioConnection,
        ports: &AvailablePorts,
        claimed_output_ports: &HashMap<(String, u32), usize>,
    ) -> (AudioConnectionStatus, Option<String>) {
        if !connection.enabled {
            return (AudioConnectionStatus::Disabled, None);
        }
        let Some(device_id) = connection.device_id.as_deref() else {
            return (AudioConnectionStatus::Disconnected, None);
        };
        if ports.has_device(device_id) {
            return (
                Self::port_status(connection, ports, None, claimed_output_ports),
                None,
            );
        }
        // A track loopback whose track is gone is missing, full stop. Standing
        // in with the hardware device at the same port numbers would record
        // the microphone under the name of the track the user picked.
        if DirectAudio::loopback::is_loopback_device(device_id) {
            return (AudioConnectionStatus::DeviceMissing, None);
        }
        let Some(stand_in) = ports.stand_in(connection.direction) else {
            return (AudioConnectionStatus::DeviceMissing, None);
        };
        match Self::port_status(connection, ports, Some(stand_in), claimed_output_ports) {
            AudioConnectionStatus::Active => {
                (AudioConnectionStatus::StandIn, Some(stand_in.to_string()))
            }
            // The stand-in cannot carry this bus (too few ports, or they are
            // taken): the bus's own device is still what is missing.
            _ => (AudioConnectionStatus::DeviceMissing, None),
        }
    }

    /// Port-level status of a bus whose device is present — its own, or the
    /// `stand_in` it runs on.
    fn port_status(
        connection: &AudioConnection,
        ports: &AvailablePorts,
        stand_in: Option<&str>,
        claimed_output_ports: &HashMap<(String, u32), usize>,
    ) -> AudioConnectionStatus {
        let channels = connection.channel_layout.channel_count();
        if connection.port_bindings.len() < channels {
            return AudioConnectionStatus::PortMissing;
        }

        let mut resolved = Vec::with_capacity(channels);
        for channel in 0..channels {
            let Some(binding) = connection.binding(channel) else {
                return AudioConnectionStatus::PortMissing;
            };
            let Some(port) = ports.resolve_binding(binding, connection.direction, stand_in) else {
                return AudioConnectionStatus::PortMissing;
            };
            resolved.push((port.device_id.clone(), port.port_index));
        }

        // Two logical channels of the same bus landing on one physical port is
        // a mistake for a stereo bus (it would collapse the image silently).
        let distinct: HashSet<_> = resolved.iter().collect();
        if channels > 1 && distinct.len() != resolved.len() {
            return AudioConnectionStatus::Conflict;
        }

        // Duplicate hardware writes: two output buses on the same port would
        // sum the same signal twice into one destination.
        if connection.direction == AudioConnectionDirection::Output
            && resolved
                .iter()
                .any(|port| claimed_output_ports.contains_key(port))
        {
            return AudioConnectionStatus::Conflict;
        }

        AudioConnectionStatus::Active
    }

    /// Physical ports this connection resolves to, in logical channel order.
    /// `None` when the connection is not currently usable — callers must not
    /// fall back to a guess.
    pub fn resolved_ports(
        &self,
        id: &AudioConnectionId,
        ports: &AvailablePorts,
    ) -> Option<Vec<u32>> {
        let connection = self.get(id)?;
        if !connection.status.is_usable() {
            return None;
        }
        let channels = connection.channel_layout.channel_count();
        let stand_in = connection.stand_in_device.as_deref();
        let mut resolved = Vec::with_capacity(channels);
        for channel in 0..channels {
            let binding = connection.binding(channel)?;
            let port = ports.resolve_binding(binding, connection.direction, stand_in)?;
            resolved.push(port.port_index);
        }
        Some(resolved)
    }

    /// Connections valid as an input for a track with `track_channels`.
    ///
    /// A mono track prefers mono buses and a stereo track prefers stereo, but a
    /// mono bus feeding a stereo track is a legitimate conversion, so it is
    /// offered after the exact matches rather than hidden.
    pub fn input_choices_for(&self, track_channels: usize) -> Vec<&AudioConnection> {
        let mut exact = Vec::new();
        let mut convertible = Vec::new();
        for connection in self.by_direction(AudioConnectionDirection::Input) {
            let channels = connection.channel_layout.channel_count();
            if channels == track_channels {
                exact.push(connection);
            } else if channels < track_channels {
                // Upmixing mono into a stereo track is well-defined.
                convertible.push(connection);
            }
        }
        exact.extend(convertible);
        exact
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "System Audio Device", 4, 4)
    }

    fn registry() -> AudioConnectionRegistry {
        AudioConnectionRegistry::default_template(&device(), "dev-1")
    }

    // ── CRUD ────────────────────────────────────────────────────────────────

    #[test]
    fn input_connections_can_be_added_renamed_edited_and_removed() {
        let ports = device();
        let mut registry = AudioConnectionRegistry::new();

        let id = registry.add(
            AudioConnection::new(
                "Microphone",
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
            )
            .bind_consecutive("dev-1", 0, |i| format!("Input {}", i + 1)),
        );
        registry.revalidate(&ports);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Active
        );

        assert!(registry.rename(&id, "Vocal Mic"));
        assert_eq!(registry.name_of(&id), Some("Vocal Mic"));

        assert!(registry.set_channel_layout(&id, ChannelLayout::Stereo));
        registry.revalidate(&ports);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::PortMissing,
            "growing to stereo leaves the new channel unbound rather than guessing"
        );

        assert!(registry.bind_port(
            &id,
            1,
            AudioPortId::new("dev-1", "Input 2", 1),
            AudioConnectionDirection::Input
        ));
        registry.revalidate(&ports);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Active
        );

        assert!(registry.remove(&id).is_some());
        assert!(registry.get(&id).is_none());
    }

    #[test]
    fn output_connections_can_be_added_renamed_edited_and_removed() {
        let ports = device();
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(
            AudioConnection::new(
                "Headphones",
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 2, |i| format!("Output {}", i + 1)),
        );
        registry.revalidate(&ports);

        let connection = registry.get(&id).unwrap();
        assert_eq!(connection.status, AudioConnectionStatus::Active);
        assert_eq!(connection.port_summary(), "Output 3 / Output 4");

        assert!(registry.rename(&id, "Cue Mix"));
        assert_eq!(registry.name_of(&id), Some("Cue Mix"));
        assert!(registry.remove(&id).is_some());
        assert!(registry
            .by_direction(AudioConnectionDirection::Output)
            .is_empty());
    }

    #[test]
    fn duplicate_names_are_disambiguated_rather_than_rejected() {
        let mut registry = AudioConnectionRegistry::new();
        let a = registry.add(AudioConnection::new(
            "Guitar",
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
        ));
        let b = registry.add(AudioConnection::new(
            "Guitar",
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
        ));
        assert_eq!(registry.name_of(&a), Some("Guitar"));
        assert_eq!(registry.name_of(&b), Some("Guitar 2"));
        assert_ne!(a, b, "ids must stay distinct regardless of name collision");
    }

    #[test]
    fn duplicating_a_connection_gives_a_fresh_id_and_keeps_the_routing() {
        let mut registry = registry();
        let source = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        let copy = registry.duplicate(&source).expect("duplicate");

        assert_ne!(copy, source);
        let original = registry.get(&source).unwrap().clone();
        let duplicated = registry.get(&copy).unwrap();
        assert_eq!(duplicated.port_bindings, original.port_bindings);
        assert_ne!(duplicated.name, original.name);
    }

    // ── Identity and stability ──────────────────────────────────────────────

    #[test]
    fn ids_are_stable_and_independent_of_the_display_name() {
        let mut registry = registry();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        let bindings = registry.get(&id).unwrap().port_bindings.clone();

        assert!(registry.rename(&id, "Completely Different Name"));
        assert_eq!(
            registry.get(&id).unwrap().port_bindings,
            bindings,
            "renaming must not disturb routing"
        );
        assert_eq!(registry.get(&id).unwrap().id, id);
    }

    // ── Defaults ────────────────────────────────────────────────────────────

    #[test]
    fn default_template_only_creates_rows_for_ports_that_exist() {
        let stereo_only = AvailablePorts::for_device("dev-1", "Stereo Interface", 1, 2);
        let registry = AudioConnectionRegistry::default_template(&stereo_only, "dev-1");

        let inputs = registry.by_direction(AudioConnectionDirection::Input);
        assert_eq!(
            inputs.len(),
            1,
            "one input port means one mono bus, no stereo bus"
        );
        assert_eq!(inputs[0].name, "Mono Input 1");

        let outputs = registry.by_direction(AudioConnectionDirection::Output);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].name, "Main Output 1-2");
        assert_eq!(outputs[0].status, AudioConnectionStatus::Active);
    }

    #[test]
    fn default_connections_resolve_selected_device_name_to_stable_id() {
        let ports = AvailablePorts::for_device("asio:{6f1c}", "Studio USB ASIO Driver", 2, 2);
        let registry = AudioConnectionRegistry::from_default_template(
            &crate::settings::DefaultAudioConnectionsSettings::default(),
            &ports,
            "Studio USB ASIO Driver",
            "Studio USB ASIO Driver",
        );

        assert!(!registry.is_empty());
        assert!(registry.all().iter().all(|connection| {
            connection.device_id.as_deref() == Some("asio:{6f1c}")
                && connection.status == AudioConnectionStatus::Active
        }));
    }

    #[test]
    fn default_template_matches_the_documented_starting_set() {
        let registry = registry();
        let inputs: Vec<_> = registry
            .by_direction(AudioConnectionDirection::Input)
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(
            inputs,
            vec!["Mono Input 1", "Mono Input 2", "Stereo Input 1-2"]
        );
        let outputs: Vec<_> = registry
            .by_direction(AudioConnectionDirection::Output)
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(outputs, vec!["Main Output 1-2"]);
    }

    // ── Channel mapping ─────────────────────────────────────────────────────

    #[test]
    fn a_mono_connection_maps_exactly_one_physical_channel() {
        let ports = device();
        let registry = registry();
        let mono = registry
            .by_direction(AudioConnectionDirection::Input)
            .into_iter()
            .find(|c| c.channel_layout == ChannelLayout::Mono)
            .unwrap();

        assert_eq!(mono.channel_layout.channel_count(), 1);
        assert_eq!(mono.port_bindings.len(), 1);
        assert_eq!(
            registry.resolved_ports(&mono.id, &ports),
            Some(vec![0]),
            "Mono Input 1 must resolve to exactly device port 0"
        );
    }

    #[test]
    fn a_stereo_connection_maps_left_and_right_to_distinct_ports() {
        let ports = device();
        let registry = registry();
        let stereo = registry
            .by_direction(AudioConnectionDirection::Input)
            .into_iter()
            .find(|c| c.channel_layout == ChannelLayout::Stereo)
            .unwrap();

        assert_eq!(stereo.binding(0).unwrap().physical_port_id.port_index, 0);
        assert_eq!(stereo.binding(1).unwrap().physical_port_id.port_index, 1);
        assert_eq!(stereo.channel_layout.channel_label(0), "Left");
        assert_eq!(stereo.channel_layout.channel_label(1), "Right");
        assert_eq!(
            registry.resolved_ports(&stereo.id, &ports),
            Some(vec![0, 1])
        );
    }

    #[test]
    fn stereo_left_and_right_on_the_same_port_is_a_conflict() {
        let ports = device();
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(AudioConnection::new(
            "Broken Stereo",
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
        ));
        registry.bind_port(
            &id,
            0,
            AudioPortId::new("dev-1", "Input 1", 0),
            AudioConnectionDirection::Input,
        );
        registry.bind_port(
            &id,
            1,
            AudioPortId::new("dev-1", "Input 1", 0),
            AudioConnectionDirection::Input,
        );
        registry.revalidate(&ports);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Conflict
        );
    }

    #[test]
    fn the_same_physical_input_may_feed_several_logical_input_buses() {
        let ports = device();
        let mut registry = AudioConnectionRegistry::new();
        for name in ["Mic A", "Mic B"] {
            registry.add(
                AudioConnection::new(name, AudioConnectionDirection::Input, ChannelLayout::Mono)
                    .bind_consecutive("dev-1", 0, |i| format!("Input {}", i + 1)),
            );
        }
        registry.revalidate(&ports);
        assert!(
            registry
                .by_direction(AudioConnectionDirection::Input)
                .iter()
                .all(|c| c.status == AudioConnectionStatus::Active),
            "sharing one physical input across logical input buses is legal"
        );
    }

    #[test]
    fn two_output_buses_on_the_same_port_are_reported_as_a_conflict() {
        let ports = device();
        let mut registry = AudioConnectionRegistry::new();
        for name in ["Main", "Duplicate Main"] {
            registry.add(
                AudioConnection::new(
                    name,
                    AudioConnectionDirection::Output,
                    ChannelLayout::Stereo,
                )
                .bind_consecutive("dev-1", 0, |i| format!("Output {}", i + 1)),
            );
        }
        registry.revalidate(&ports);
        let outputs = registry.by_direction(AudioConnectionDirection::Output);
        assert_eq!(outputs[0].status, AudioConnectionStatus::Active);
        assert_eq!(
            outputs[1].status,
            AudioConnectionStatus::Conflict,
            "a duplicate hardware write must be detected, not silently doubled"
        );
    }

    #[test]
    fn an_input_bus_cannot_bind_an_output_port_and_vice_versa() {
        let mut registry = AudioConnectionRegistry::new();
        let input = registry.add(AudioConnection::new(
            "In",
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
        ));
        let output = registry.add(AudioConnection::new(
            "Out",
            AudioConnectionDirection::Output,
            ChannelLayout::Mono,
        ));

        assert!(
            !registry.bind_port(
                &input,
                0,
                AudioPortId::new("dev-1", "Output 1", 0),
                AudioConnectionDirection::Output
            ),
            "an input bus must reject an output port"
        );
        assert!(!registry.bind_port(
            &output,
            0,
            AudioPortId::new("dev-1", "Input 1", 0),
            AudioConnectionDirection::Input
        ));
    }

    // ── Device loss and restore ─────────────────────────────────────────────

    #[test]
    fn a_missing_device_preserves_connections_and_their_bindings() {
        let mut registry = registry();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        let bindings = registry.get(&id).unwrap().port_bindings.clone();

        registry.revalidate(&AvailablePorts::default());
        let connection = registry.get(&id).expect("connection must survive");
        assert_eq!(connection.status, AudioConnectionStatus::DeviceMissing);
        assert_eq!(
            connection.port_bindings, bindings,
            "bindings must be preserved so the device can come back"
        );
    }

    #[test]
    fn reconnecting_the_same_device_restores_the_mapping() {
        let ports = device();
        let mut registry = registry();
        let id = registry.by_direction(AudioConnectionDirection::Input)[2]
            .id
            .clone();
        let before = registry.resolved_ports(&id, &ports);
        assert_eq!(before, Some(vec![0, 1]));

        registry.revalidate(&AvailablePorts::default());
        assert_eq!(
            registry.resolved_ports(&id, &AvailablePorts::default()),
            None
        );

        registry.revalidate(&ports);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Active
        );
        assert_eq!(registry.resolved_ports(&id, &ports), before);
    }

    // ── Stand-in device ─────────────────────────────────────────────────────

    /// What a project saved on an ASIO driver meets under WASAPI: its device
    /// is gone, and Audio Device Setup has a USB interface selected.
    fn usb_selected(inputs: u32) -> AvailablePorts {
        AvailablePorts::for_device("usb", "USB Audio", inputs, 2)
            .with_stand_ins("USB Audio", "USB Audio")
    }

    #[test]
    fn a_missing_device_runs_on_the_selected_device_at_the_same_ports() {
        let ports = usb_selected(2);
        let mut registry = registry();
        let input = registry.by_direction(AudioConnectionDirection::Input)[2]
            .id
            .clone();
        let output = registry.by_direction(AudioConnectionDirection::Output)[0]
            .id
            .clone();
        let bindings = registry.get(&input).unwrap().port_bindings.clone();

        registry.revalidate(&ports);

        let connection = registry.get(&input).unwrap();
        assert_eq!(connection.status, AudioConnectionStatus::StandIn);
        assert!(connection.status.is_usable());
        assert_eq!(connection.effective_device_id(), Some("usb"));
        assert_eq!(connection.device_id.as_deref(), Some("dev-1"));
        assert_eq!(
            connection.port_bindings, bindings,
            "a stand-in must not rewrite what the project stores"
        );
        assert_eq!(registry.resolved_ports(&input, &ports), Some(vec![0, 1]));
        assert_eq!(
            registry.get(&output).unwrap().status,
            AudioConnectionStatus::StandIn
        );
        assert_eq!(registry.resolved_ports(&output, &ports), Some(vec![0, 1]));
    }

    #[test]
    fn the_assigned_device_takes_over_again_when_it_returns() {
        let mut registry = registry();
        let id = registry.by_direction(AudioConnectionDirection::Input)[2]
            .id
            .clone();
        registry.revalidate(&usb_selected(2));

        let ports = device().merge(usb_selected(2));
        registry.revalidate(&ports);

        let connection = registry.get(&id).unwrap();
        assert_eq!(connection.status, AudioConnectionStatus::Active);
        assert_eq!(connection.stand_in_device, None);
        assert_eq!(connection.effective_device_id(), Some("dev-1"));
    }

    #[test]
    fn a_stand_in_without_the_ports_leaves_the_bus_missing() {
        let ports = usb_selected(1);
        let mut registry = registry();
        let stereo = registry.by_direction(AudioConnectionDirection::Input)[2]
            .id
            .clone();

        registry.revalidate(&ports);

        let connection = registry.get(&stereo).unwrap();
        assert_eq!(connection.status, AudioConnectionStatus::DeviceMissing);
        assert_eq!(connection.stand_in_device, None);
        assert_eq!(registry.resolved_ports(&stereo, &ports), None);
    }

    /// A track loopback bus whose track was deleted must not run on the
    /// interface at the same port numbers: that would record the microphone
    /// under the name of the track the user chose.
    #[test]
    fn a_loopback_bus_never_stands_in_on_hardware() {
        let ports = usb_selected(2);
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(
            AudioConnection::new(
                "WDW In",
                AudioConnectionDirection::Input,
                ChannelLayout::Stereo,
            )
            .bind_consecutive(
                DirectAudio::loopback::loopback_device_id("deleted"),
                0,
                |i| if i == 0 { "L" } else { "R" }.to_string(),
            ),
        );

        registry.revalidate(&ports);

        let connection = registry.get(&id).unwrap();
        assert_eq!(connection.status, AudioConnectionStatus::DeviceMissing);
        assert_eq!(connection.stand_in_device, None);
        assert_eq!(registry.resolved_ports(&id, &ports), None);
    }

    #[test]
    fn there_is_no_stand_in_without_a_present_selection() {
        let unselected = AvailablePorts::for_device("usb", "USB Audio", 2, 2);
        assert_eq!(unselected.with_stand_ins("", " ").stand_in_input, None);
        assert_eq!(
            AvailablePorts::for_device("usb", "USB Audio", 2, 2)
                .with_stand_ins("Unplugged Mic", "Unplugged Speakers")
                .stand_in_output,
            None
        );

        let mut registry = registry();
        registry.revalidate(&AvailablePorts::for_device("usb", "USB Audio", 2, 2));
        assert!(registry
            .all()
            .iter()
            .all(|connection| connection.status == AudioConnectionStatus::DeviceMissing));
    }

    #[test]
    fn moving_to_another_stand_in_rebuilds_routing() {
        let mut registry = registry();
        registry.revalidate(&usb_selected(2));

        let other = AvailablePorts::for_device("usb", "USB Audio", 2, 2)
            .merge(AvailablePorts::for_device("usb-2", "Other USB", 2, 2))
            .with_stand_ins("Other USB", "Other USB");
        let mutation = registry.validate_all(&other);

        assert!(mutation.needs_routing_rebuild);
        let input = registry.by_direction(AudioConnectionDirection::Input)[2];
        assert_eq!(input.status, AudioConnectionStatus::StandIn);
        assert_eq!(input.effective_device_id(), Some("usb-2"));
    }

    /// Restoration keys on the stable port *name* first, so a driver that
    /// re-orders its ports cannot silently move a bus onto another input.
    #[test]
    fn reordered_device_ports_follow_the_port_name_not_the_index() {
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(
            AudioConnection::new(
                "Guitar",
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
            )
            .bind_consecutive("dev-1", 1, |i| format!("Input {}", i + 1)),
        );
        // The driver comes back with "Input 2" now sitting at index 3.
        let reordered = AvailablePorts {
            ports: vec![
                AvailablePort {
                    device_id: "dev-1".into(),
                    device_name: "System Audio Device".into(),
                    port_name: "Input 1".into(),
                    port_index: 0,
                    direction: AudioConnectionDirection::Input,
                },
                AvailablePort {
                    device_id: "dev-1".into(),
                    device_name: "System Audio Device".into(),
                    port_name: "Input 2".into(),
                    port_index: 3,
                    direction: AudioConnectionDirection::Input,
                },
            ],
            ..AvailablePorts::default()
        };
        registry.revalidate(&reordered);
        assert_eq!(
            registry.resolved_ports(&id, &reordered),
            Some(vec![3]),
            "the bus should follow its named port, not stay on index 1"
        );
    }

    #[test]
    fn a_disabled_connection_reports_disabled_and_is_not_usable() {
        let ports = device();
        let mut registry = registry();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        assert!(registry.set_enabled(&id, false));
        registry.revalidate(&ports);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Disabled
        );
        assert_eq!(registry.resolved_ports(&id, &ports), None);
    }

    #[test]
    fn changing_device_rebinds_surviving_ports_and_drops_the_rest() {
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(
            AudioConnection::new(
                "Stereo In",
                AudioConnectionDirection::Input,
                ChannelLayout::Stereo,
            )
            .bind_consecutive("dev-1", 0, |i| format!("Input {}", i + 1)),
        );
        // The new device only has one input.
        let small = AvailablePorts::for_device("dev-2", "Tiny Interface", 1, 2);
        assert!(registry.set_device(&id, "dev-2", &small));
        registry.revalidate(&small);

        let connection = registry.get(&id).unwrap();
        assert_eq!(connection.device_id.as_deref(), Some("dev-2"));
        assert_eq!(connection.port_bindings.len(), 1);
        assert_eq!(
            connection.status,
            AudioConnectionStatus::PortMissing,
            "the right channel has nowhere to go and must be reported, not guessed"
        );
    }

    // ── Track-facing choices ────────────────────────────────────────────────

    #[test]
    fn input_choices_prefer_the_matching_layout_and_allow_mono_into_stereo() {
        let registry = registry();

        let mono = registry.input_choices_for(1);
        assert!(mono.iter().all(|c| c.channel_layout == ChannelLayout::Mono));

        let stereo = registry.input_choices_for(2);
        assert_eq!(
            stereo[0].channel_layout,
            ChannelLayout::Stereo,
            "an exact stereo match must be offered first"
        );
        assert!(
            stereo
                .iter()
                .any(|c| c.channel_layout == ChannelLayout::Mono),
            "mono into a stereo track is a valid conversion and stays available"
        );
    }

    #[test]
    fn input_choices_never_include_output_connections() {
        let registry = registry();
        for channels in [1usize, 2] {
            assert!(
                registry
                    .input_choices_for(channels)
                    .iter()
                    .all(|c| c.direction == AudioConnectionDirection::Input),
                "an output bus must never be offered as a track input"
            );
        }
    }

    // ── Invalid state never panics ──────────────────────────────────────────

    #[test]
    fn operations_on_an_unknown_id_fail_softly() {
        let ports = device();
        let mut registry = registry();
        let ghost = AudioConnectionId::from_stored("ac-does-not-exist");

        assert!(registry.get(&ghost).is_none());
        assert!(registry.name_of(&ghost).is_none());
        assert!(!registry.rename(&ghost, "x"));
        assert!(registry.duplicate(&ghost).is_none());
        assert!(registry.remove(&ghost).is_none());
        assert!(!registry.set_enabled(&ghost, false));
        assert!(!registry.set_channel_layout(&ghost, ChannelLayout::Mono));
        assert!(!registry.clear_binding(&ghost, 0));
        assert!(!registry.set_device(&ghost, "dev-1", &ports));
        assert!(registry.resolved_ports(&ghost, &ports).is_none());
        assert!(!registry.bind_port(
            &ghost,
            0,
            AudioPortId::new("dev-1", "Input 1", 0),
            AudioConnectionDirection::Input
        ));
    }

    #[test]
    fn binding_a_channel_outside_the_layout_is_rejected() {
        let mut registry = AudioConnectionRegistry::new();
        let id = registry.add(AudioConnection::new(
            "Mono",
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
        ));
        assert!(!registry.bind_port(
            &id,
            1,
            AudioPortId::new("dev-1", "Input 2", 1),
            AudioConnectionDirection::Input
        ));
    }

    #[test]
    fn a_custom_layout_carries_its_own_channel_count() {
        let layout = ChannelLayout::Custom { channels: 6 };
        assert_eq!(layout.channel_count(), 6);
        assert_eq!(layout.channel_label(4), "Ch 5");
        // Degenerate counts are clamped rather than allowed to produce a
        // zero-channel bus the engine would have to special-case.
        assert_eq!(ChannelLayout::Custom { channels: 0 }.channel_count(), 1);
    }

    #[test]
    fn reset_to_defaults_rebuilds_the_template() {
        let ports = device();
        let mut registry = registry();
        let extra = registry.add(AudioConnection::new(
            "Custom Bus",
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
        ));
        assert!(registry.get(&extra).is_some());

        registry.reset_to_defaults(&ports, "dev-1");
        assert!(registry.get(&extra).is_none());
        assert_eq!(registry.len(), 4);
    }
}

/// A raw `(device, ordered channels)` selection, used **only** to convert legacy
/// project routing into logical buses.
///
/// No runtime UI produces one of these any more: Add Track and the Inspector
/// both select an existing [`AudioConnection`] and store its id
/// ([`crate::input_routing`]). This type survives because a pre-v34 project
/// *does* store raw device/channel routing, and loading one has to turn that
/// into connections exactly once — see [`crate::project::routing_migration`].
///
/// Do not reintroduce it into a selector: two competing input-selection systems
/// is precisely what the centralization removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysicalInputChoice {
    /// No Input.
    None,
    /// A concrete device + ordered channel list. Order is semantic
    /// (Left, Right) and is never sorted or normalized.
    Ports {
        device_id: String,
        channels: Vec<u32>,
    },
}

impl AudioConnectionRegistry {
    /// Resolve a legacy raw input selection to a logical connection id,
    /// creating a project-local connection when none matches.
    ///
    /// Matching is exact on `(device_id, ordered channel list)`, so a legacy
    /// project whose tracks shared one physical input migrates onto one bus
    /// instead of accumulating duplicates.
    ///
    /// **Migration only.** No selector may call this: a user picking an input
    /// chooses an existing bus, and creating one is an explicit Audio
    /// Connections action.
    pub fn get_or_create_audio_connection_for_physical_input(
        &mut self,
        choice: &PhysicalInputChoice,
        ports: &AvailablePorts,
    ) -> Option<AudioConnectionId> {
        let (device_id, channels) = match choice {
            PhysicalInputChoice::None => return None,
            PhysicalInputChoice::Ports {
                device_id,
                channels,
            } if !channels.is_empty() => (device_id, channels),
            PhysicalInputChoice::Ports { .. } => return None,
        };
        let layout = match channels.len() {
            1 => ChannelLayout::Mono,
            2 => ChannelLayout::Stereo,
            n => ChannelLayout::Custom { channels: n },
        };
        if let Some(existing) = self.connections.iter().find(|connection| {
            connection.direction == AudioConnectionDirection::Input
                && connection.device_id.as_deref() == Some(device_id.as_str())
                && connection.channel_layout == layout
                && connection.port_bindings.len() == channels.len()
                && channels.iter().enumerate().all(|(logical, physical)| {
                    connection
                        .binding(logical)
                        .is_some_and(|binding| binding.physical_port_id.port_index == *physical)
                })
        }) {
            return Some(existing.id.clone());
        }
        let name = generated_input_name(channels, ports.device_name(device_id));
        let mut connection = AudioConnection::new(name, AudioConnectionDirection::Input, layout);
        connection.device_id = Some(device_id.clone());
        connection.port_bindings = channels
            .iter()
            .enumerate()
            .map(|(logical_channel, physical)| {
                let port_name = ports
                    .ports_for(device_id, AudioConnectionDirection::Input)
                    .into_iter()
                    .find(|port| port.port_index == *physical)
                    .map(|port| port.port_name.clone())
                    .unwrap_or_else(|| format!("Input {}", physical + 1));
                AudioPortBinding {
                    logical_channel,
                    physical_port_id: AudioPortId::new(device_id, port_name, *physical),
                }
            })
            .collect();
        let id = self.add(connection);
        self.revalidate(ports);
        Some(id)
    }
}

/// Shared naming for generated input connections, used by both the v33
/// migration and the Inspector/Add Track bridge so they cannot drift.
///
/// The result is a *logical* bus name and deliberately carries no hardware
/// endpoint description. The device and its ports are shown in their own
/// columns; folding "Mic/Inst/Line In 1/2 (Studio 24c)" into the name is what
/// made the routing model unreadable. `device_name` is accepted only so the
/// signature stays stable for callers that still pass it.
pub fn generated_input_name(channels: &[u32], _device_name: Option<&str>) -> String {
    let ports: Vec<_> = channels.iter().map(|c| c + 1).collect();
    match ports.as_slice() {
        [single] => format!("Input {single}"),
        [left, right] if *right == left + 1 => format!("Stereo Input {left}-{right}"),
        many => format!(
            "Input {}",
            many.iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join("+")
        ),
    }
}

/// Build [`AvailablePorts`] from the process-wide device registry cache, so
/// every caller validates against the same view of the hardware.
pub fn current_available_ports() -> AvailablePorts {
    let snapshot = crate::device_registry::audio_snapshot();
    let mut ports = AvailablePorts::default();
    for device in &snapshot.inputs {
        ports = ports.merge(AvailablePorts::for_device(
            &device.id,
            &device.name,
            device.channels,
            0,
        ));
    }
    for device in &snapshot.outputs {
        ports = ports.merge(AvailablePorts::for_device(
            &device.id,
            &device.name,
            0,
            device.channels,
        ));
    }
    // Stand-ins are the devices the engine actually opens: the Audio Device
    // Setup pick, or the system default when that is left empty (which is
    // what changing the driver leaves behind).
    let (selected_input, selected_output) = crate::device_registry::selected_audio_devices();
    let or_default = |selected: String, default: &Option<String>| {
        if selected.is_empty() {
            default.clone().unwrap_or_default()
        } else {
            selected
        }
    };
    let ports = ports.with_stand_ins(
        &or_default(selected_input, &snapshot.default_input),
        &or_default(selected_output, &snapshot.default_output),
    );
    // Audio Jam streams are input ports too. Routing a remote performer to a
    // track therefore goes through this same layer — stable ids, non-destructive
    // loss when they leave, one place that maps logical to physical — instead of
    // a second routing model living beside it.
    // Track loopback sources are input ports too: an instrument track feeding
    // an audio track is the same routing gesture as an interface channel
    // feeding one, and goes through this same layer rather than a private path
    // beside it.
    ports
        .merge(crate::jam::available_ports())
        .merge(crate::loopback_ports::available_ports())
}

// ── Structured mutation API ─────────────────────────────────────────────────
//
// Panel code never reaches into the registry's internals. Every edit goes
// through one of these methods and reports what changed, so the caller knows
// whether to republish runtime routing and which tracks were touched.

/// Outcome of one registry edit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionMutation {
    /// Connections whose stored data changed.
    pub changed: Vec<AudioConnectionId>,
    /// Tracks whose input assignment was cleared as a consequence.
    pub affected_track_ids: Vec<String>,
    /// Human-readable notes for the panel's warning area.
    pub warnings: Vec<String>,
    /// Whether the runtime routing snapshot must be recompiled. A name-only
    /// edit leaves this false — routing identity is the id, not the name.
    pub needs_routing_rebuild: bool,
}

impl ConnectionMutation {
    fn nothing() -> Self {
        Self::default()
    }

    fn routing(id: AudioConnectionId) -> Self {
        Self {
            changed: vec![id],
            needs_routing_rebuild: true,
            ..Self::default()
        }
    }

    pub fn did_change(&self) -> bool {
        !self.changed.is_empty() || !self.affected_track_ids.is_empty()
    }
}

/// Channel layouts the panel offers. Surround is deliberately absent from the
/// UI while [`ChannelLayout::Custom`] stays in the model.
pub const PANEL_LAYOUTS: [ChannelLayout; 2] = [ChannelLayout::Mono, ChannelLayout::Stereo];

/// A ready-made bus set for the Audio Connections page.
///
/// A preset describes **both directions at once**, because that is how an
/// interface is described: "2×2" is two ins and two outs, not two buses on
/// whichever tab happens to be in front. Applying one replaces the project's
/// buses, so the window always confirms first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPreset {
    /// `channels` in and `channels` out, laid out as stereo pairs: 2×2 is one
    /// pair each way, 8×8 is four.
    StereoPairs { channels: usize },
    /// Two mono buses plus one stereo bus over the same first two ports — the
    /// small-interface setup, where the two inputs are recorded separately or
    /// as a pair without re-patching.
    TwoMonoOneStereo,
    /// One mono bus per port the device exposes.
    AllMono,
    /// One stereo bus per consecutive pair of ports.
    AllStereo,
}

/// Presets the toolbar offers, in menu order.
pub const PANEL_PRESETS: [ConnectionPreset; 6] = [
    ConnectionPreset::StereoPairs { channels: 2 },
    ConnectionPreset::StereoPairs { channels: 4 },
    ConnectionPreset::StereoPairs { channels: 8 },
    ConnectionPreset::TwoMonoOneStereo,
    ConnectionPreset::AllMono,
    ConnectionPreset::AllStereo,
];

impl ConnectionPreset {
    /// Menu label. `2 × 2` rather than `2x2`, to read as a channel count and
    /// not as a file name.
    pub fn label(self) -> String {
        match self {
            Self::StereoPairs { channels } => format!("{channels} × {channels}"),
            Self::TwoMonoOneStereo => "2 Mono + 1 Stereo".to_string(),
            Self::AllMono => "All Mono".to_string(),
            Self::AllStereo => "All Stereo".to_string(),
        }
    }

    /// The buses this preset makes for one direction on one device.
    ///
    /// A preset never invents hardware: 8×8 on a two-input interface produces
    /// the pairs that fit and nothing else, the same rule `default_template`
    /// follows. Bindings come from the ports the device actually reports, so a
    /// driver whose ports are not named `Input 1…n` still binds correctly.
    pub fn connections(
        self,
        direction: AudioConnectionDirection,
        ports: &AvailablePorts,
        device_id: &str,
    ) -> Vec<AudioConnection> {
        let mut available = ports.ports_for(device_id, direction);
        available.sort_by_key(|port| port.port_index);
        let count = available.len();

        let mut built = Vec::new();
        let push = |first: usize, layout: ChannelLayout, built: &mut Vec<AudioConnection>| {
            if let Some(connection) = preset_bus(direction, layout, device_id, &available, first) {
                built.push(connection);
            }
        };

        match self {
            Self::StereoPairs { channels } => {
                for first in (0..channels.min(count)).step_by(2) {
                    push(first, ChannelLayout::Stereo, &mut built);
                }
            }
            Self::TwoMonoOneStereo => {
                for first in 0..2.min(count) {
                    push(first, ChannelLayout::Mono, &mut built);
                }
                push(0, ChannelLayout::Stereo, &mut built);
            }
            Self::AllMono => {
                for first in 0..count {
                    push(first, ChannelLayout::Mono, &mut built);
                }
            }
            Self::AllStereo => {
                for first in (0..count).step_by(2) {
                    push(first, ChannelLayout::Stereo, &mut built);
                }
            }
        }
        built
    }
}

/// One preset bus, bound to `layout.channel_count()` consecutive ports starting
/// at `first`. `None` when the device does not expose that many ports, which is
/// how a preset trims itself to the hardware.
///
fn preset_bus(
    direction: AudioConnectionDirection,
    layout: ChannelLayout,
    device_id: &str,
    available: &[&AvailablePort],
    first: usize,
) -> Option<AudioConnection> {
    let channels = layout.channel_count();
    let slice = available.get(first..first + channels)?;
    let name = preset_bus_name(direction, layout, first, channels);
    let mut connection = AudioConnection::new(name, direction, layout);
    connection.device_id = Some(device_id.to_string());
    connection.port_bindings = slice
        .iter()
        .enumerate()
        .map(|(channel, port)| AudioPortBinding {
            logical_channel: channel,
            physical_port_id: AudioPortId::new(
                &port.device_id,
                port.port_name.clone(),
                port.port_index,
            ),
        })
        .collect();
    Some(connection)
}

/// Bus name for a preset row, in the same style as the default template:
/// `Mono Input 1`, `Stereo Input 1-2`, `Main Output 1-2`, `Stereo Output 3-4`.
///
/// The stereo bus over the first two output ports is the Main Output, whichever
/// preset built it — that is the bus a project's Master is expected to find.
fn preset_bus_name(
    direction: AudioConnectionDirection,
    layout: ChannelLayout,
    first: usize,
    channels: usize,
) -> String {
    let first_channel = first + 1;
    let last_channel = first + channels;
    let range = if channels > 1 {
        format!("{first_channel}-{last_channel}")
    } else {
        format!("{first_channel}")
    };
    match (direction, layout) {
        (AudioConnectionDirection::Input, ChannelLayout::Mono) => format!("Mono Input {range}"),
        (AudioConnectionDirection::Input, _) => format!("Stereo Input {range}"),
        (AudioConnectionDirection::Output, ChannelLayout::Mono) => format!("Mono Output {range}"),
        (AudioConnectionDirection::Output, _) if first == 0 => format!("Main Output {range}"),
        (AudioConnectionDirection::Output, _) => format!("Stereo Output {range}"),
    }
}

impl AudioConnectionRegistry {
    /// The display name of a bus's device — the human-readable endpoint name
    /// when the device is present, the stored id as a last resort so a
    /// disconnected assignment still shows *something* identifiable.
    ///
    /// Kept separate from the bus name on purpose: the name column is the
    /// user's logical label, this is the hardware behind it.
    pub fn device_display_name(
        &self,
        id: &AudioConnectionId,
        ports: &AvailablePorts,
    ) -> Option<String> {
        let device_id = self.get(id)?.device_id.clone()?;
        Some(
            ports
                .device_name(&device_id)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .unwrap_or(device_id),
        )
    }

    /// Create a bus with a unique default name and no port assignment.
    ///
    /// A new bus is never auto-bound to hardware: it starts `Disconnected` so
    /// the user chooses its ports deliberately, and a new output can never
    /// silently collide with an existing one.
    pub fn add_connection(
        &mut self,
        direction: AudioConnectionDirection,
        layout: ChannelLayout,
        ports: &AvailablePorts,
    ) -> (AudioConnectionId, ConnectionMutation) {
        let base = match (direction, layout) {
            (AudioConnectionDirection::Input, ChannelLayout::Mono) => "Mono Input",
            (AudioConnectionDirection::Input, _) => "Stereo Input",
            (AudioConnectionDirection::Output, ChannelLayout::Mono) => "Mono Output",
            (AudioConnectionDirection::Output, _) => "Stereo Output",
        };
        // `add` disambiguates the name ("Stereo Input", "Stereo Input 2", …),
        // so the default stays concise and logical without a device prefix.
        let id = self.add(AudioConnection::new(base, direction, layout));
        self.revalidate(ports);
        (id.clone(), ConnectionMutation::routing(id))
    }

    /// Rename a bus. Whitespace is trimmed; an empty name is rejected so a row
    /// can never become unlabelled.
    ///
    /// Never sets `needs_routing_rebuild`: routing identity is the id.
    pub fn update_name(&mut self, id: &AudioConnectionId, name: &str) -> ConnectionMutation {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return ConnectionMutation {
                warnings: vec!["A connection name cannot be empty.".to_string()],
                ..ConnectionMutation::nothing()
            };
        }
        if self.get(id).is_none() {
            return ConnectionMutation::nothing();
        }
        if self.get(id).map(|c| c.name.as_str()) == Some(trimmed) {
            return ConnectionMutation::nothing();
        }
        self.rename(id, trimmed);
        ConnectionMutation {
            changed: vec![id.clone()],
            needs_routing_rebuild: false,
            ..ConnectionMutation::nothing()
        }
    }

    /// Change the channel layout.
    ///
    /// Mono to Stereo keeps the existing mono binding as Left and leaves Right
    /// unassigned; Stereo to Mono keeps Left and drops Right. Neither direction
    /// invents a port.
    pub fn update_layout(
        &mut self,
        id: &AudioConnectionId,
        layout: ChannelLayout,
        ports: &AvailablePorts,
    ) -> ConnectionMutation {
        let Some(connection) = self.get(id) else {
            return ConnectionMutation::nothing();
        };
        if connection.channel_layout == layout {
            return ConnectionMutation::nothing();
        }
        let dropped_right = connection.channel_layout.channel_count() > layout.channel_count()
            && connection.binding(1).is_some();
        self.set_channel_layout(id, layout);
        self.revalidate(ports);
        let mut mutation = ConnectionMutation::routing(id.clone());
        if dropped_right {
            mutation
                .warnings
                .push("The Right channel assignment was removed.".to_string());
        }
        mutation
    }

    /// Point a bus at a different device.
    ///
    /// Bindings the new device cannot satisfy are dropped rather than
    /// reinterpreted — a numeric index means nothing across unrelated devices.
    pub fn update_device(
        &mut self,
        id: &AudioConnectionId,
        device_id: Option<&str>,
        ports: &AvailablePorts,
    ) -> ConnectionMutation {
        let Some(connection) = self.get(id) else {
            return ConnectionMutation::nothing();
        };
        if connection.device_id.as_deref() == device_id {
            return ConnectionMutation::nothing();
        }
        match device_id {
            Some(device_id) => {
                self.set_device(id, device_id, ports);
            }
            None => {
                if let Some(connection) = self.get_mut(id) {
                    connection.device_id = None;
                    connection.port_bindings.clear();
                }
            }
        }
        self.revalidate(ports);
        ConnectionMutation::routing(id.clone())
    }

    /// Assign one logical channel to a physical port, or clear it.
    pub fn update_port_binding(
        &mut self,
        id: &AudioConnectionId,
        logical_channel: usize,
        port: Option<AudioPortId>,
        ports: &AvailablePorts,
    ) -> ConnectionMutation {
        let Some(connection) = self.get(id) else {
            return ConnectionMutation::nothing();
        };
        let direction = connection.direction;
        let changed = match port {
            Some(port) => self.bind_port(id, logical_channel, port, direction),
            None => self.clear_binding(id, logical_channel),
        };
        if !changed {
            return ConnectionMutation::nothing();
        }
        self.revalidate(ports);
        let mut mutation = ConnectionMutation::routing(id.clone());
        if self.get(id).map(|c| c.status) == Some(AudioConnectionStatus::Conflict) {
            mutation
                .warnings
                .push("This mapping conflicts with another assignment.".to_string());
        }
        mutation
    }

    /// Enable or disable a bus. Bindings are preserved either way — a disabled
    /// bus keeps every mapping and simply compiles to silence.
    pub fn update_enabled(
        &mut self,
        id: &AudioConnectionId,
        enabled: bool,
        ports: &AvailablePorts,
    ) -> ConnectionMutation {
        if self.get(id).map(|c| c.enabled) == Some(enabled) {
            return ConnectionMutation::nothing();
        }
        if !self.set_enabled(id, enabled) {
            return ConnectionMutation::nothing();
        }
        self.revalidate(ports);
        ConnectionMutation::routing(id.clone())
    }

    /// Copy a bus, including its ordered bindings, under a new id and name.
    ///
    /// A duplicated output that now collides keeps its mapping and reports
    /// `Conflict` rather than being silently moved.
    pub fn duplicate_connection(
        &mut self,
        id: &AudioConnectionId,
        ports: &AvailablePorts,
    ) -> (Option<AudioConnectionId>, ConnectionMutation) {
        let Some(new_id) = self.duplicate(id) else {
            return (None, ConnectionMutation::nothing());
        };
        self.revalidate(ports);
        let mut mutation = ConnectionMutation::routing(new_id.clone());
        if self.get(&new_id).map(|c| c.status) == Some(AudioConnectionStatus::Conflict) {
            mutation.warnings.push(
                "The duplicate writes the same hardware ports as another output.".to_string(),
            );
        }
        (Some(new_id), mutation)
    }

    /// Remove a bus. `referencing_tracks` is what the caller found via
    /// [`AudioConnectionRegistry::device_choices`]-adjacent lookup on the
    /// project; those ids are reported back so the caller can unassign them.
    /// This method never touches tracks itself.
    pub fn remove_connection(
        &mut self,
        id: &AudioConnectionId,
        referencing_tracks: &[String],
        ports: &AvailablePorts,
    ) -> ConnectionMutation {
        if self.remove(id).is_none() {
            return ConnectionMutation::nothing();
        }
        self.revalidate(ports);
        ConnectionMutation {
            changed: vec![id.clone()],
            affected_track_ids: referencing_tracks.to_vec(),
            warnings: Vec::new(),
            needs_routing_rebuild: true,
        }
    }

    /// Replace one direction's buses with the defaults derivable from
    /// `device_id`. The other direction is untouched, so resetting Inputs
    /// cannot disturb Outputs.
    pub fn reset_defaults_for(
        &mut self,
        direction: AudioConnectionDirection,
        ports: &AvailablePorts,
        selected_device: &str,
        referencing_tracks: &[String],
    ) -> ConnectionMutation {
        let removed: Vec<AudioConnectionId> = self
            .by_direction(direction)
            .into_iter()
            .map(|connection| connection.id.clone())
            .collect();
        for id in &removed {
            self.remove(id);
        }
        let device_id = ports
            .matching_device_id(selected_device, direction)
            .unwrap_or(selected_device);
        let template = Self::default_template(ports, device_id);
        let added: Vec<AudioConnection> = template
            .all()
            .iter()
            .filter(|connection| connection.direction == direction)
            .cloned()
            .collect();
        let mut changed = removed;
        for connection in added {
            changed.push(self.add(connection));
        }
        self.revalidate(ports);
        ConnectionMutation {
            changed,
            affected_track_ids: referencing_tracks.to_vec(),
            warnings: Vec::new(),
            needs_routing_rebuild: true,
        }
    }

    /// Replace every bus in the project with one preset's bus set.
    ///
    /// Both directions at once: a preset names an interface shape, so applying
    /// "4 × 4" while the Inputs tab is in front must still build the four
    /// outputs. A direction whose device is not selected — or does not expose
    /// enough ports — contributes nothing and says so in the warnings, rather
    /// than creating rows that could only ever read Port Missing.
    pub fn apply_preset(
        &mut self,
        preset: ConnectionPreset,
        ports: &AvailablePorts,
        input_device: &str,
        output_device: &str,
        referencing_tracks: &[String],
    ) -> ConnectionMutation {
        let mut changed: Vec<AudioConnectionId> = self
            .all()
            .iter()
            .map(|connection| connection.id.clone())
            .collect();
        for id in changed.clone() {
            self.remove(&id);
        }

        let mut warnings = Vec::new();
        for (direction, selected) in [
            (AudioConnectionDirection::Input, input_device),
            (AudioConnectionDirection::Output, output_device),
        ] {
            let label = direction.tag();
            let Some(device_id) = ports
                .matching_device_id(selected, direction)
                .map(str::to_string)
            else {
                warnings.push(format!(
                    "No {label} device is available, so no {label} buses were created."
                ));
                continue;
            };
            let built = preset.connections(direction, ports, &device_id);
            if built.is_empty() {
                warnings.push(format!(
                    "The {label} device exposes too few ports for this preset."
                ));
            }
            for connection in built {
                changed.push(self.add(connection));
            }
        }

        self.revalidate(ports);
        ConnectionMutation {
            changed,
            affected_track_ids: referencing_tracks.to_vec(),
            warnings,
            needs_routing_rebuild: true,
        }
    }

    /// Bind every bus in one direction to consecutive ports on the current
    /// device, in table order and honouring each bus's own layout.
    ///
    /// Auto Map never creates or removes a bus: it answers "wire what I already
    /// have to the hardware", which is the part that is tedious by hand on an
    /// eight-input interface. A bus the remaining ports cannot fill is left
    /// unbound and named in the warnings rather than half-bound, because a
    /// stereo bus with only a Left port is worse than an honest Unassigned.
    pub fn auto_map(
        &mut self,
        direction: AudioConnectionDirection,
        ports: &AvailablePorts,
        selected_device: &str,
    ) -> ConnectionMutation {
        let label = direction.tag();
        let Some(device_id) = ports
            .matching_device_id(selected_device, direction)
            .map(str::to_string)
        else {
            return ConnectionMutation {
                warnings: vec![format!("No {label} device is available to map to.")],
                ..ConnectionMutation::nothing()
            };
        };
        let mut available = ports.ports_for(&device_id, direction);
        available.sort_by_key(|port| port.port_index);

        let ids: Vec<AudioConnectionId> = self
            .by_direction(direction)
            .into_iter()
            .map(|connection| connection.id.clone())
            .collect();
        let mut cursor = 0usize;
        let mut changed = Vec::new();
        let mut unfilled = Vec::new();
        for id in ids {
            let Some(connection) = self.get_mut(&id) else {
                continue;
            };
            let channels = connection.channel_layout.channel_count();
            connection.device_id = Some(device_id.clone());
            match available.get(cursor..cursor + channels) {
                Some(slice) => {
                    connection.port_bindings = slice
                        .iter()
                        .enumerate()
                        .map(|(channel, port)| AudioPortBinding {
                            logical_channel: channel,
                            physical_port_id: AudioPortId::new(
                                &port.device_id,
                                port.port_name.clone(),
                                port.port_index,
                            ),
                        })
                        .collect();
                    cursor += channels;
                }
                None => {
                    connection.port_bindings.clear();
                    unfilled.push(connection.name.clone());
                }
            }
            changed.push(id);
        }

        let mut warnings = Vec::new();
        if !unfilled.is_empty() {
            warnings.push(format!(
                "The {label} device has only {} port(s); left unassigned: {}.",
                available.len(),
                unfilled.join(", ")
            ));
        }
        self.revalidate(ports);
        let needs_routing_rebuild = !changed.is_empty();
        ConnectionMutation {
            changed,
            affected_track_ids: Vec::new(),
            warnings,
            needs_routing_rebuild,
        }
    }

    /// Recompute every status. Exposed so a device-inventory change can refresh
    /// the panel without going through an edit.
    pub fn validate_all(&mut self, ports: &AvailablePorts) -> ConnectionMutation {
        // A bus moving from one stand-in device to another keeps its status
        // but not its route, so the device is part of the comparison.
        let before: Vec<_> = self
            .all()
            .iter()
            .map(|c| (c.id.clone(), c.status, c.stand_in_device.clone()))
            .collect();
        self.revalidate(ports);
        let changed: Vec<_> = self
            .all()
            .iter()
            .filter(|c| {
                before
                    .iter()
                    .find(|(id, _, _)| *id == c.id)
                    .map(|(_, status, stand_in)| {
                        *status != c.status || *stand_in != c.stand_in_device
                    })
                    .unwrap_or(true)
            })
            .map(|c| c.id.clone())
            .collect();
        let needs_routing_rebuild = !changed.is_empty();
        ConnectionMutation {
            changed,
            needs_routing_rebuild,
            ..ConnectionMutation::nothing()
        }
    }

    /// Devices the panel may offer for this bus, as `(device_id, label,
    /// available)`. The currently assigned device is included even when it is
    /// missing, so a disconnected assignment stays visible in the dropdown
    /// rather than silently vanishing.
    pub fn device_choices(
        &self,
        id: &AudioConnectionId,
        ports: &AvailablePorts,
    ) -> Vec<(String, String, bool)> {
        let direction = match self.get(id) {
            Some(connection) => connection.direction,
            None => return Vec::new(),
        };
        let mut seen: Vec<(String, String, bool)> = Vec::new();
        for port in &ports.ports {
            if port.direction != direction {
                continue;
            }
            if !seen
                .iter()
                .any(|(existing, _, _)| *existing == port.device_id)
            {
                seen.push((port.device_id.clone(), port.device_name.clone(), true));
            }
        }
        if let Some(assigned) = self.get(id).and_then(|c| c.device_id.clone()) {
            if !seen.iter().any(|(existing, _, _)| *existing == assigned) {
                let label = format!("{assigned} (unavailable)");
                seen.push((assigned, label, false));
            }
        }
        seen
    }

    /// Ports the panel may offer for one logical channel of this bus, as
    /// `(port_id, label, available)`. Includes the currently bound port even
    /// when it is missing.
    pub fn port_choices(
        &self,
        id: &AudioConnectionId,
        logical_channel: usize,
        ports: &AvailablePorts,
    ) -> Vec<(AudioPortId, String, bool)> {
        let Some(connection) = self.get(id) else {
            return Vec::new();
        };
        let Some(device_id) = connection.device_id.as_deref() else {
            return Vec::new();
        };
        let mut choices: Vec<(AudioPortId, String, bool)> = ports
            .ports_for(device_id, connection.direction)
            .into_iter()
            .map(|port| {
                (
                    AudioPortId::new(&port.device_id, &port.port_name, port.port_index),
                    port.port_name.clone(),
                    true,
                )
            })
            .collect();
        if let Some(bound) = connection.binding(logical_channel) {
            let port = &bound.physical_port_id;
            if !choices.iter().any(|(existing, _, _)| existing == port) {
                let label = format!("{} (unavailable)", port.port_name);
                choices.push((port.clone(), label, false));
            }
        }
        choices
    }
}

#[cfg(test)]
mod mutation_api_tests {
    use super::*;

    fn ports() -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "Interface", 4, 4)
    }

    fn registry() -> AudioConnectionRegistry {
        AudioConnectionRegistry::default_template(&ports(), "dev-1")
    }

    fn input_id(registry: &AudioConnectionRegistry, name: &str) -> AudioConnectionId {
        registry
            .by_direction(AudioConnectionDirection::Input)
            .into_iter()
            .find(|c| c.name == name)
            .expect("connection")
            .id
            .clone()
    }

    // ── Add ─────────────────────────────────────────────────────────────────

    #[test]
    fn adding_buses_mints_unique_ids_and_disambiguated_names() {
        let mut registry = AudioConnectionRegistry::new();
        let (a, _) = registry.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
            &ports(),
        );
        let (b, _) = registry.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
            &ports(),
        );
        assert_ne!(a, b);
        assert_eq!(registry.name_of(&a), Some("Mono Input"));
        assert_eq!(registry.name_of(&b), Some("Mono Input 2"));
    }

    #[test]
    fn each_direction_and_layout_can_be_added() {
        let mut registry = AudioConnectionRegistry::new();
        for (direction, layout, expected) in [
            (
                AudioConnectionDirection::Input,
                ChannelLayout::Mono,
                "Mono Input",
            ),
            (
                AudioConnectionDirection::Input,
                ChannelLayout::Stereo,
                "Stereo Input",
            ),
            (
                AudioConnectionDirection::Output,
                ChannelLayout::Mono,
                "Mono Output",
            ),
            (
                AudioConnectionDirection::Output,
                ChannelLayout::Stereo,
                "Stereo Output",
            ),
        ] {
            let (id, mutation) = registry.add_connection(direction, layout, &ports());
            let connection = registry.get(&id).unwrap();
            assert_eq!(connection.name, expected);
            assert_eq!(connection.direction, direction);
            assert_eq!(connection.channel_layout, layout);
            assert!(connection.enabled);
            assert!(mutation.needs_routing_rebuild);
        }
    }

    /// A new bus must not grab hardware on its own — that is how two outputs
    /// would silently end up on one port.
    #[test]
    fn a_new_bus_starts_unassigned_and_disconnected() {
        let mut registry = registry();
        let (id, _) = registry.add_connection(
            AudioConnectionDirection::Output,
            ChannelLayout::Stereo,
            &ports(),
        );
        let connection = registry.get(&id).unwrap();
        assert!(connection.device_id.is_none());
        assert!(connection.port_bindings.is_empty());
        assert_eq!(connection.status, AudioConnectionStatus::Disconnected);
    }

    // ── Rename ──────────────────────────────────────────────────────────────

    #[test]
    fn renaming_trims_and_never_rebuilds_routing() {
        let mut registry = registry();
        let id = input_id(&registry, "Mono Input 1");
        let mutation = registry.update_name(&id, "   Microphone  ");
        assert_eq!(registry.name_of(&id), Some("Microphone"));
        assert!(!mutation.needs_routing_rebuild);
        assert_eq!(mutation.changed, vec![id]);
    }

    #[test]
    fn an_empty_rename_is_rejected_with_a_warning() {
        let mut registry = registry();
        let id = input_id(&registry, "Mono Input 1");
        let mutation = registry.update_name(&id, "   ");
        assert_eq!(registry.name_of(&id), Some("Mono Input 1"));
        assert!(!mutation.did_change());
        assert_eq!(mutation.warnings.len(), 1);
    }

    // ── Layout ──────────────────────────────────────────────────────────────

    #[test]
    fn mono_to_stereo_keeps_left_and_leaves_right_unassigned() {
        let mut registry = registry();
        let id = input_id(&registry, "Mono Input 1");
        let mutation = registry.update_layout(&id, ChannelLayout::Stereo, &ports());

        let connection = registry.get(&id).unwrap();
        assert_eq!(
            connection.binding(0).unwrap().physical_port_id.port_index,
            0
        );
        assert!(
            connection.binding(1).is_none(),
            "Right must not be guessed from an adjacent channel"
        );
        assert_eq!(connection.status, AudioConnectionStatus::PortMissing);
        assert!(mutation.needs_routing_rebuild);
    }

    #[test]
    fn stereo_to_mono_keeps_left_drops_right_and_says_so() {
        let mut registry = registry();
        let id = input_id(&registry, "Stereo Input 1-2");
        let mutation = registry.update_layout(&id, ChannelLayout::Mono, &ports());

        let connection = registry.get(&id).unwrap();
        assert_eq!(
            connection.binding(0).unwrap().physical_port_id.port_index,
            0
        );
        assert!(connection.binding(1).is_none());
        assert_eq!(connection.status, AudioConnectionStatus::Active);
        assert_eq!(mutation.warnings.len(), 1);
    }

    // ── Device / ports ──────────────────────────────────────────────────────

    #[test]
    fn changing_device_drops_bindings_the_new_device_cannot_satisfy() {
        let mut registry = registry();
        let id = input_id(&registry, "Stereo Input 1-2");
        let small = AvailablePorts::for_device("dev-2", "Tiny", 1, 2);

        let mutation = registry.update_device(&id, Some("dev-2"), &small);
        let connection = registry.get(&id).unwrap();
        assert_eq!(connection.device_id.as_deref(), Some("dev-2"));
        assert_eq!(connection.port_bindings.len(), 1);
        assert_eq!(connection.status, AudioConnectionStatus::PortMissing);
        assert!(mutation.needs_routing_rebuild);
    }

    #[test]
    fn clearing_the_device_clears_the_bindings() {
        let mut registry = registry();
        let id = input_id(&registry, "Stereo Input 1-2");
        registry.update_device(&id, None, &ports());
        let connection = registry.get(&id).unwrap();
        assert!(connection.device_id.is_none());
        assert!(connection.port_bindings.is_empty());
        assert_eq!(connection.status, AudioConnectionStatus::Disconnected);
    }

    #[test]
    fn stereo_left_and_right_keep_their_order_and_stay_distinct() {
        let mut registry = registry();
        let id = input_id(&registry, "Stereo Input 1-2");
        registry.update_port_binding(
            &id,
            0,
            Some(AudioPortId::new("dev-1", "Input 3", 2)),
            &ports(),
        );
        registry.update_port_binding(
            &id,
            1,
            Some(AudioPortId::new("dev-1", "Input 2", 1)),
            &ports(),
        );
        assert_eq!(registry.resolved_ports(&id, &ports()), Some(vec![2, 1]));
    }

    #[test]
    fn binding_left_and_right_to_one_port_reports_a_conflict() {
        let mut registry = registry();
        let id = input_id(&registry, "Stereo Input 1-2");
        let mutation = registry.update_port_binding(
            &id,
            1,
            Some(AudioPortId::new("dev-1", "Input 1", 0)),
            &ports(),
        );
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Conflict
        );
        assert_eq!(mutation.warnings.len(), 1);
    }

    #[test]
    fn port_and_device_choices_keep_a_missing_assignment_visible() {
        let mut registry = registry();
        let id = input_id(&registry, "Mono Input 1");
        registry.revalidate(&AvailablePorts::default());

        let devices = registry.device_choices(&id, &AvailablePorts::default());
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].0, "dev-1");
        assert!(!devices[0].2, "the missing device is flagged unavailable");

        let choices = registry.port_choices(&id, 0, &AvailablePorts::default());
        assert_eq!(choices.len(), 1);
        assert!(!choices[0].2);
        assert!(choices[0].1.contains("unavailable"));
    }

    // ── Enable / disable ────────────────────────────────────────────────────

    #[test]
    fn disabling_preserves_mappings_and_reenabling_restores_validation() {
        let mut registry = registry();
        let id = input_id(&registry, "Stereo Input 1-2");
        let before = registry.get(&id).unwrap().port_bindings.clone();

        let mutation = registry.update_enabled(&id, false, &ports());
        assert!(mutation.needs_routing_rebuild);
        let connection = registry.get(&id).unwrap();
        assert_eq!(connection.status, AudioConnectionStatus::Disabled);
        assert_eq!(connection.port_bindings, before, "mappings are kept");
        assert_eq!(
            registry.resolved_ports(&id, &ports()),
            None,
            "a disabled bus compiles to silence"
        );

        registry.update_enabled(&id, true, &ports());
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Active
        );
        assert_eq!(registry.resolved_ports(&id, &ports()), Some(vec![0, 1]));
    }

    // ── Duplicate ───────────────────────────────────────────────────────────

    #[test]
    fn duplicating_copies_mappings_under_a_new_id_and_name() {
        let mut registry = registry();
        let id = input_id(&registry, "Stereo Input 1-2");
        let (copy, _) = registry.duplicate_connection(&id, &ports());
        let copy = copy.expect("duplicate");

        assert_ne!(copy, id);
        let original = registry.get(&id).unwrap().clone();
        let duplicate = registry.get(&copy).unwrap();
        assert_eq!(duplicate.port_bindings, original.port_bindings);
        assert_eq!(duplicate.direction, original.direction);
        assert_eq!(duplicate.enabled, original.enabled);
        assert_ne!(duplicate.name, original.name);
    }

    /// A duplicated output collides by definition — it must say so rather than
    /// being quietly moved to free ports.
    #[test]
    fn a_duplicated_output_reports_the_conflict_and_keeps_its_mapping() {
        let mut registry = registry();
        let id = registry.by_direction(AudioConnectionDirection::Output)[0]
            .id
            .clone();
        let (copy, mutation) = registry.duplicate_connection(&id, &ports());
        let copy = copy.unwrap();

        assert_eq!(
            registry.get(&copy).unwrap().status,
            AudioConnectionStatus::Conflict
        );
        assert_eq!(registry.resolved_ports(&copy, &ports()), None);
        assert_eq!(mutation.warnings.len(), 1);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Active,
            "the original is untouched"
        );
    }

    // ── Remove ──────────────────────────────────────────────────────────────

    #[test]
    fn removing_reports_the_referencing_tracks_it_was_given() {
        let mut registry = registry();
        let id = input_id(&registry, "Mono Input 1");
        let mutation = registry.remove_connection(
            &id,
            &["track-1".to_string(), "track-2".to_string()],
            &ports(),
        );
        assert!(registry.get(&id).is_none());
        assert_eq!(mutation.affected_track_ids, vec!["track-1", "track-2"]);
        assert!(mutation.needs_routing_rebuild);
    }

    #[test]
    fn removing_an_unknown_connection_changes_nothing() {
        let mut registry = registry();
        let before = registry.len();
        let mutation =
            registry.remove_connection(&AudioConnectionId::from_stored("ghost"), &[], &ports());
        assert_eq!(registry.len(), before);
        assert!(!mutation.did_change());
        assert!(!mutation.needs_routing_rebuild);
    }

    // ── Reset ───────────────────────────────────────────────────────────────

    #[test]
    fn resetting_one_direction_leaves_the_other_untouched() {
        let mut registry = registry();
        let output_before = registry.by_direction(AudioConnectionDirection::Output)[0]
            .id
            .clone();
        registry.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
            &ports(),
        );

        let mutation = registry.reset_defaults_for(
            AudioConnectionDirection::Input,
            &ports(),
            "dev-1",
            &["track-1".to_string()],
        );

        let inputs: Vec<_> = registry
            .by_direction(AudioConnectionDirection::Input)
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(
            inputs,
            vec!["Mono Input 1", "Mono Input 2", "Stereo Input 1-2"]
        );
        assert!(
            registry.get(&output_before).is_some(),
            "resetting Inputs must not disturb Outputs"
        );
        assert_eq!(mutation.affected_track_ids, vec!["track-1"]);
        assert!(mutation.needs_routing_rebuild);
    }

    #[test]
    fn resetting_only_creates_rows_for_ports_that_exist() {
        let tiny = AvailablePorts::for_device("dev-1", "Tiny", 1, 2);
        let mut registry = registry();
        registry.reset_defaults_for(AudioConnectionDirection::Input, &tiny, "dev-1", &[]);
        let inputs = registry.by_direction(AudioConnectionDirection::Input);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].name, "Mono Input 1");
    }

    #[test]
    fn resetting_with_no_device_produces_no_rows_and_does_not_panic() {
        let mut registry = registry();
        registry.reset_defaults_for(
            AudioConnectionDirection::Input,
            &AvailablePorts::default(),
            "nothing",
            &[],
        );
        assert!(registry
            .by_direction(AudioConnectionDirection::Input)
            .is_empty());
    }

    // ── Device inventory changes ────────────────────────────────────────────

    #[test]
    fn validate_all_reports_status_changes_when_a_device_disappears_and_returns() {
        let mut registry = registry();
        let id = input_id(&registry, "Mono Input 1");

        let lost = registry.validate_all(&AvailablePorts::default());
        assert!(lost.needs_routing_rebuild);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::DeviceMissing
        );

        let restored = registry.validate_all(&ports());
        assert!(restored.needs_routing_rebuild);
        assert_eq!(
            registry.get(&id).unwrap().status,
            AudioConnectionStatus::Active
        );
        assert_eq!(registry.resolved_ports(&id, &ports()), Some(vec![0]));

        // A second pass with no change reports nothing to rebuild.
        let idle = registry.validate_all(&ports());
        assert!(!idle.needs_routing_rebuild);
    }

    #[test]
    fn mutations_on_an_unknown_id_are_inert() {
        let mut registry = registry();
        let ghost = AudioConnectionId::from_stored("ghost");
        assert!(!registry.update_name(&ghost, "x").did_change());
        assert!(!registry
            .update_layout(&ghost, ChannelLayout::Mono, &ports())
            .did_change());
        assert!(!registry
            .update_device(&ghost, Some("dev-1"), &ports())
            .did_change());
        assert!(!registry
            .update_port_binding(&ghost, 0, None, &ports())
            .did_change());
        assert!(!registry
            .update_enabled(&ghost, false, &ports())
            .did_change());
        assert!(registry.duplicate_connection(&ghost, &ports()).0.is_none());
    }
}

#[cfg(test)]
mod preset_and_auto_map_tests {
    use super::*;

    fn ports(inputs: u32, outputs: u32) -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "Interface", inputs, outputs)
    }

    fn names(
        registry: &AudioConnectionRegistry,
        direction: AudioConnectionDirection,
    ) -> Vec<String> {
        registry
            .by_direction(direction)
            .iter()
            .map(|connection| connection.name.clone())
            .collect()
    }

    fn bound_ports(connection: &AudioConnection) -> Vec<u32> {
        let mut ordered: Vec<_> = connection.port_bindings.iter().collect();
        ordered.sort_by_key(|binding| binding.logical_channel);
        ordered
            .iter()
            .map(|binding| binding.physical_port_id.port_index)
            .collect()
    }

    /// The menu is built straight from this list, so its order and its labels
    /// are the product surface.
    #[test]
    fn the_preset_menu_offers_the_advertised_set() {
        let labels: Vec<String> = PANEL_PRESETS.iter().map(|preset| preset.label()).collect();
        assert_eq!(
            labels,
            vec![
                "2 × 2",
                "4 × 4",
                "8 × 8",
                "2 Mono + 1 Stereo",
                "All Mono",
                "All Stereo"
            ]
        );
    }

    /// The whole point of the preset menu: "4 × 4" is an interface shape, so it
    /// has to build the outputs even though the Inputs tab is what was in front
    /// when it was picked.
    #[test]
    fn a_preset_builds_both_directions_at_once() {
        let ports = ports(8, 8);
        let mut registry = AudioConnectionRegistry::new();

        registry.apply_preset(
            ConnectionPreset::StereoPairs { channels: 4 },
            &ports,
            "dev-1",
            "dev-1",
            &[],
        );

        assert_eq!(
            names(&registry, AudioConnectionDirection::Input),
            vec!["Stereo Input 1-2", "Stereo Input 3-4"]
        );
        assert_eq!(
            names(&registry, AudioConnectionDirection::Output),
            vec!["Main Output 1-2", "Stereo Output 3-4"]
        );
        let inputs = registry.by_direction(AudioConnectionDirection::Input);
        assert_eq!(bound_ports(inputs[0]), vec![0, 1]);
        assert_eq!(bound_ports(inputs[1]), vec![2, 3]);
    }

    /// A preset never invents hardware — the same rule the default template
    /// follows, so 8 × 8 on a 2-in/2-out box is one pair each way.
    #[test]
    fn a_preset_trims_itself_to_the_ports_the_device_has() {
        let ports = ports(2, 2);
        let mut registry = AudioConnectionRegistry::new();

        let mutation = registry.apply_preset(
            ConnectionPreset::StereoPairs { channels: 8 },
            &ports,
            "dev-1",
            "dev-1",
            &[],
        );

        assert_eq!(
            names(&registry, AudioConnectionDirection::Input),
            vec!["Stereo Input 1-2"]
        );
        assert_eq!(
            names(&registry, AudioConnectionDirection::Output),
            vec!["Main Output 1-2"]
        );
        assert!(mutation.warnings.is_empty(), "one pair still fits");
    }

    #[test]
    fn two_mono_one_stereo_covers_the_first_pair_both_ways() {
        let ports = ports(2, 2);
        let mut registry = AudioConnectionRegistry::new();

        registry.apply_preset(
            ConnectionPreset::TwoMonoOneStereo,
            &ports,
            "dev-1",
            "dev-1",
            &[],
        );

        assert_eq!(
            names(&registry, AudioConnectionDirection::Input),
            vec!["Mono Input 1", "Mono Input 2", "Stereo Input 1-2"]
        );
        assert_eq!(
            names(&registry, AudioConnectionDirection::Output),
            vec!["Mono Output 1", "Mono Output 2", "Main Output 1-2"]
        );
    }

    #[test]
    fn all_mono_and_all_stereo_follow_the_port_count() {
        let ports = ports(4, 4);

        let mut mono = AudioConnectionRegistry::new();
        mono.apply_preset(ConnectionPreset::AllMono, &ports, "dev-1", "dev-1", &[]);
        assert_eq!(
            names(&mono, AudioConnectionDirection::Input),
            vec![
                "Mono Input 1",
                "Mono Input 2",
                "Mono Input 3",
                "Mono Input 4"
            ]
        );

        let mut stereo = AudioConnectionRegistry::new();
        stereo.apply_preset(ConnectionPreset::AllStereo, &ports, "dev-1", "dev-1", &[]);
        assert_eq!(
            names(&stereo, AudioConnectionDirection::Input),
            vec!["Stereo Input 1-2", "Stereo Input 3-4"]
        );
    }

    /// Applying a preset is a replacement, so the caller has to be told which
    /// track references it just invalidated — the same contract as a removal.
    #[test]
    fn applying_a_preset_replaces_the_existing_buses_and_reports_the_tracks() {
        let ports = ports(4, 4);
        let mut registry = AudioConnectionRegistry::default_template(&ports, "dev-1");
        let before = registry.len();
        assert!(before > 0);

        let mutation = registry.apply_preset(
            ConnectionPreset::StereoPairs { channels: 2 },
            &ports,
            "dev-1",
            "dev-1",
            &["track-1".to_string()],
        );

        assert_eq!(
            names(&registry, AudioConnectionDirection::Input),
            vec!["Stereo Input 1-2"]
        );
        assert_eq!(mutation.affected_track_ids, vec!["track-1"]);
        assert!(mutation.needs_routing_rebuild);
        assert!(
            mutation.changed.len() >= before,
            "every old bus is a change"
        );
    }

    #[test]
    fn a_preset_with_no_device_creates_nothing_and_says_why() {
        let mut registry = AudioConnectionRegistry::new();

        let mutation = registry.apply_preset(
            ConnectionPreset::StereoPairs { channels: 2 },
            &AvailablePorts::default(),
            "nothing",
            "nothing",
            &[],
        );

        assert!(registry.is_empty());
        assert_eq!(mutation.warnings.len(), 2, "one per direction");
    }

    // ── Auto Map ────────────────────────────────────────────────────────────

    /// Auto Map walks the table in order and gives each bus as many consecutive
    /// ports as its own layout needs.
    #[test]
    fn auto_map_binds_buses_in_order_and_honours_each_layout() {
        let ports = ports(4, 4);
        let mut registry = AudioConnectionRegistry::new();
        registry.add(AudioConnection::new(
            "A",
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
        ));
        registry.add(AudioConnection::new(
            "B",
            AudioConnectionDirection::Input,
            ChannelLayout::Mono,
        ));

        let mutation = registry.auto_map(AudioConnectionDirection::Input, &ports, "dev-1");

        let inputs = registry.by_direction(AudioConnectionDirection::Input);
        assert_eq!(bound_ports(inputs[0]), vec![0, 1]);
        assert_eq!(bound_ports(inputs[1]), vec![2]);
        assert!(mutation.warnings.is_empty());
        assert!(mutation.needs_routing_rebuild);
    }

    /// Half a stereo bus is worse than an honest Unassigned, so a bus the
    /// remaining ports cannot fill is left unbound and named.
    #[test]
    fn auto_map_leaves_a_bus_it_cannot_fill_unbound_and_warns() {
        let ports = ports(3, 3);
        let mut registry = AudioConnectionRegistry::new();
        for name in ["A", "B"] {
            registry.add(AudioConnection::new(
                name,
                AudioConnectionDirection::Input,
                ChannelLayout::Stereo,
            ));
        }

        let mutation = registry.auto_map(AudioConnectionDirection::Input, &ports, "dev-1");

        let inputs = registry.by_direction(AudioConnectionDirection::Input);
        assert_eq!(bound_ports(inputs[0]), vec![0, 1]);
        assert!(
            inputs[1].port_bindings.is_empty(),
            "a stereo bus is never half-bound"
        );
        assert_eq!(mutation.warnings.len(), 1);
        assert!(mutation.warnings[0].contains('B'));
    }

    /// Auto Map answers "wire what I have", never "give me different buses".
    #[test]
    fn auto_map_creates_and_removes_nothing_and_leaves_the_other_direction_alone() {
        let ports = ports(4, 4);
        let mut registry = AudioConnectionRegistry::default_template(&ports, "dev-1");
        let inputs_before = names(&registry, AudioConnectionDirection::Input);
        let outputs_before: Vec<Vec<u32>> = registry
            .by_direction(AudioConnectionDirection::Output)
            .iter()
            .map(|connection| bound_ports(connection))
            .collect();

        registry.auto_map(AudioConnectionDirection::Input, &ports, "dev-1");

        assert_eq!(
            names(&registry, AudioConnectionDirection::Input),
            inputs_before
        );
        let outputs_after: Vec<Vec<u32>> = registry
            .by_direction(AudioConnectionDirection::Output)
            .iter()
            .map(|connection| bound_ports(connection))
            .collect();
        assert_eq!(outputs_after, outputs_before);
    }

    #[test]
    fn auto_map_with_no_device_changes_nothing_and_says_why() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(AudioConnection::new(
            "A",
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
        ));

        let mutation = registry.auto_map(
            AudioConnectionDirection::Input,
            &AvailablePorts::default(),
            "nothing",
        );

        assert!(!mutation.did_change());
        assert_eq!(mutation.warnings.len(), 1);
    }
}
