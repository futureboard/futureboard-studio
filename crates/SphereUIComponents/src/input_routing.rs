//! Track audio-input selection over logical Input Audio Connections.
//!
//! ```txt
//! Add Track / Inspector  ->  Input Audio Connection  ->  AudioConnectionRegistry
//!                                                            -> physical ports
//! ```
//!
//! The counterpart to [`crate::output_routing`], and the only place a track
//! input selector gets its choices. Two rules define it:
//!
//! * **No physical devices.** Neither Add Track nor the Inspector enumerates
//!   hardware. A device or channel only becomes selectable once the user has
//!   made it a logical bus in Audio Connections, which is why both selectors end
//!   with [`InputChoice::OpenAudioConnections`].
//! * **`None` is No Input.** It is never "system default input": a track with no
//!   connection captures nothing. Wanting default hardware is expressed by
//!   creating a bus for it, not by leaving the field empty.

use crate::audio_connections::{
    AudioConnection, AudioConnectionDirection, AudioConnectionId, AudioConnectionRegistry,
    AvailablePorts, ChannelLayout,
};
use crate::output_routing::UNAVAILABLE_SUFFIX;

/// Label for a track with no input connection.
pub const NO_INPUT_LABEL: &str = "No Input";

/// Label shown when the stored id is not in this project's registry at all.
pub const MISSING_CONNECTION_LABEL: &str = "Missing connection";

/// What a track Input selector asks for. Deliberately carries no device or
/// channel: the only routing value a track can be given is a connection id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackInputSelection {
    /// Capture nothing. Never a system default input.
    NoInput,
    Connection(AudioConnectionId),
    /// Not a routing change — opens the window where buses are created.
    OpenAudioConnections,
    /// An entry this track cannot take, chosen anyway. Not a routing change:
    /// the selector reports why instead.
    Refused(String),
}

/// Why a connection cannot be one particular track's input.
///
/// Such an entry stays in the menu, marked, rather than being hidden: a bus
/// that silently vanishes from one track's menu while every other track lists
/// it reads as a bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRefusal {
    /// After the name in the menu, e.g. `Feedback`.
    pub short: String,
    /// What the selector reports when the entry is chosen.
    pub detail: String,
}

/// One entry in a track Input selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputChoice {
    /// Captures nothing. Not a system default.
    NoInput,
    Connection {
        id: AudioConnectionId,
        label: String,
        /// Channel layout, so a selector can group Mono and Stereo entries.
        layout: ChannelLayout,
        /// False when this entry is only listed because it is the current
        /// assignment and cannot currently reach hardware.
        available: bool,
        /// Set when this track cannot take this connection at all.
        refused: Option<InputRefusal>,
    },
    /// Opens the Audio Connections window. Never a routing change.
    OpenAudioConnections,
}

impl InputChoice {
    pub fn label(&self) -> String {
        match self {
            Self::NoInput => NO_INPUT_LABEL.to_string(),
            Self::Connection {
                label,
                available,
                refused,
                ..
            } => {
                if let Some(refused) = refused {
                    format!("{label} — {}", refused.short)
                } else if *available {
                    label.clone()
                } else {
                    format!("{label} — {UNAVAILABLE_SUFFIX}")
                }
            }
            Self::OpenAudioConnections => "Open Audio Connections...".to_string(),
        }
    }

    pub fn connection_id(&self) -> Option<&AudioConnectionId> {
        match self {
            Self::Connection { id, .. } => Some(id),
            _ => None,
        }
    }

    /// What choosing this entry asks the layout to do.
    pub fn selection(&self) -> TrackInputSelection {
        match self {
            Self::NoInput => TrackInputSelection::NoInput,
            Self::Connection {
                refused: Some(refused),
                ..
            } => TrackInputSelection::Refused(refused.detail.clone()),
            Self::Connection { id, .. } => TrackInputSelection::Connection(id.clone()),
            Self::OpenAudioConnections => TrackInputSelection::OpenAudioConnections,
        }
    }

    /// Group heading a selector may file this entry under.
    pub fn group(&self) -> Option<&'static str> {
        match self {
            Self::Connection { layout, .. } => Some(match layout {
                ChannelLayout::Mono => "Mono",
                ChannelLayout::Stereo => "Stereo",
                ChannelLayout::Custom { .. } => "Multichannel",
            }),
            _ => None,
        }
    }
}

fn is_offerable_input(connection: &AudioConnection) -> bool {
    connection.direction == AudioConnectionDirection::Input && connection.enabled
}

/// Entries for a track Input selector.
///
/// `track_channels` orders the list the way [`AudioConnectionRegistry::input_choices_for`]
/// does — exact layout matches first, then legitimate up-mixes — without ever
/// hiding a usable bus. A stale or disabled current assignment is appended so
/// the selector shows what the track actually holds.
pub fn track_input_options(
    registry: &AudioConnectionRegistry,
    track_channels: usize,
    current: Option<&AudioConnectionId>,
) -> Vec<InputChoice> {
    track_input_options_impl(registry, track_channels, current, |_| None, false)
}

/// [`track_input_options`] for one particular track's Input selector.
///
/// `refusal` says, per connection, why this track cannot take it (a track
/// loopback that would feed the track its own output). Refused entries stay
/// listed and marked, and so do buses wider than the track — a stereo bus on a
/// mono track — so a bus never just goes missing from one track's menu.
pub fn track_input_options_for(
    registry: &AudioConnectionRegistry,
    track_channels: usize,
    current: Option<&AudioConnectionId>,
    refusal: impl Fn(&AudioConnection) -> Option<InputRefusal>,
) -> Vec<InputChoice> {
    track_input_options_impl(registry, track_channels, current, refusal, true)
}

fn layout_name(layout: ChannelLayout) -> String {
    match layout {
        ChannelLayout::Mono => "Mono".to_string(),
        ChannelLayout::Stereo => "Stereo".to_string(),
        ChannelLayout::Custom { channels } => format!("{channels}-channel"),
    }
}

/// Why a bus wider than the track is not offered as its input.
fn too_wide_refusal(connection: &AudioConnection, track_channels: usize) -> InputRefusal {
    let track_format = if track_channels <= 1 {
        "Mono"
    } else {
        "Stereo"
    };
    let bus = layout_name(connection.channel_layout);
    InputRefusal {
        short: format!("{bus}, track is {track_format}"),
        detail: format!(
            "\"{}\" is a {bus} bus and this track is {track_format}. Set the track's Audio \
             Format to match, or choose a {track_format} bus.",
            connection.name
        ),
    }
}

fn track_input_options_impl(
    registry: &AudioConnectionRegistry,
    track_channels: usize,
    current: Option<&AudioConnectionId>,
    refusal: impl Fn(&AudioConnection) -> Option<InputRefusal>,
    list_too_wide: bool,
) -> Vec<InputChoice> {
    let mut entries = vec![InputChoice::NoInput];
    let mut listed_current = false;
    for connection in registry.input_choices_for(track_channels) {
        if !is_offerable_input(connection) {
            continue;
        }
        if Some(&connection.id) == current {
            listed_current = true;
        }
        entries.push(InputChoice::Connection {
            id: connection.id.clone(),
            label: connection.name.clone(),
            layout: connection.channel_layout,
            available: connection.status.is_usable(),
            refused: refusal(connection),
        });
    }
    if list_too_wide {
        for connection in registry.by_direction(AudioConnectionDirection::Input) {
            if !is_offerable_input(connection)
                || connection.channel_layout.channel_count() <= track_channels
            {
                continue;
            }
            if Some(&connection.id) == current {
                listed_current = true;
            }
            entries.push(InputChoice::Connection {
                id: connection.id.clone(),
                label: connection.name.clone(),
                layout: connection.channel_layout,
                available: connection.status.is_usable(),
                refused: Some(
                    refusal(connection)
                        .unwrap_or_else(|| too_wide_refusal(connection, track_channels)),
                ),
            });
        }
    }

    // A disconnected device must never make the track look unassigned.
    if let Some(current) = current {
        if !listed_current {
            let connection = registry.get(current);
            let (label, layout) = connection
                .map(|connection| (connection.name.clone(), connection.channel_layout))
                .unwrap_or_else(|| (MISSING_CONNECTION_LABEL.to_string(), ChannelLayout::Mono));
            entries.push(InputChoice::Connection {
                id: current.clone(),
                label,
                layout,
                available: false,
                refused: connection.and_then(&refusal),
            });
        }
    }

    entries.push(InputChoice::OpenAudioConnections);
    entries
}

/// Menu suffix for a track loopback that would feed a track its own output.
pub const FEEDBACK_SUFFIX: &str = "Feedback";

/// Why `track_id` cannot take `connection` as its input, if it cannot.
///
/// Only a track loopback (`trk:<source>`, see [`crate::loopback_ports`]) can
/// be refused: when the track's own audio already reaches `source` — it *is*
/// the source, or its output, a send or another loopback lands there — the
/// route would hand the track its own output a block later, forever. With
/// monitoring on that is runaway feedback. The engine refuses to resolve such
/// a route too; this is so the selector can say why instead of the track
/// silently recording nothing.
pub fn loopback_refusal(
    tracks: &[crate::components::timeline::timeline_state::TrackState],
    registry: &AudioConnectionRegistry,
    track_id: &str,
    connection: &AudioConnection,
) -> Option<InputRefusal> {
    let source_id = connection
        .device_id
        .as_deref()
        .and_then(DirectAudio::loopback::loopback_track_id)?;
    let name_of = |id: &str| {
        tracks
            .iter()
            .find(|track| track.id == id)
            .map(|track| track.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    let track_name = name_of(track_id);
    if source_id == track_id {
        return Some(InputRefusal {
            short: FEEDBACK_SUFFIX.to_string(),
            detail: format!(
                "\"{track_name}\" cannot take its own output as its input: that is a feedback loop."
            ),
        });
    }
    if !audio_reaches(tracks, registry, track_id, source_id) {
        return None;
    }
    let source_name = name_of(source_id);
    Some(InputRefusal {
        short: FEEDBACK_SUFFIX.to_string(),
        detail: format!(
            "\"{track_name}\" cannot take \"{source_name}\" as its input: \"{track_name}\" already \
             feeds \"{source_name}\", so it would hear its own output (feedback)."
        ),
    })
}

/// Whether audio leaving track `from` arrives at track `to`: through its
/// output to a bus, a send, or a track loopback input on another track.
fn audio_reaches(
    tracks: &[crate::components::timeline::timeline_state::TrackState],
    registry: &AudioConnectionRegistry,
    from: &str,
    to: &str,
) -> bool {
    use crate::components::timeline::timeline_state::TrackOutputRouting;
    let loopback_source_of = |track: &crate::components::timeline::timeline_state::TrackState| {
        track
            .routing
            .audio_input_connection_id
            .as_ref()
            .and_then(|id| registry.get(id))
            .and_then(|connection| connection.device_id.as_deref())
            .and_then(DirectAudio::loopback::loopback_track_id)
            .map(str::to_string)
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut stack: Vec<String> = vec![from.to_string()];
    while let Some(at) = stack.pop() {
        if at == to {
            return true;
        }
        let Some(track) = tracks.iter().find(|track| track.id == at) else {
            continue;
        };
        if seen.contains(&track.id.as_str()) {
            continue;
        }
        seen.push(track.id.as_str());
        if let TrackOutputRouting::Bus { bus_id } = &track.routing.output {
            stack.push(bus_id.clone());
        }
        stack.extend(track.sends.iter().map(|send| send.target_track_id.clone()));
        stack.extend(
            tracks
                .iter()
                .filter(|reader| loopback_source_of(reader).as_deref() == Some(at.as_str()))
                .map(|reader| reader.id.clone()),
        );
    }
    false
}

/// Primary label for a track's input.
pub fn track_input_label(
    registry: &AudioConnectionRegistry,
    current: Option<&AudioConnectionId>,
) -> String {
    let Some(id) = current else {
        return NO_INPUT_LABEL.to_string();
    };
    match registry.get(id) {
        Some(connection) if is_offerable_input(connection) && connection.status.is_usable() => {
            connection.name.clone()
        }
        Some(connection) => format!("{} — {UNAVAILABLE_SUFFIX}", connection.name),
        // The id survives, but this project has no such bus. Say so rather than
        // showing No Input, which would misreport what the track holds.
        None => MISSING_CONNECTION_LABEL.to_string(),
    }
}

/// Secondary line for a connection: `Mono · Studio 24c · Input 1`.
///
/// Purely descriptive. The hardware behind a bus lives in the registry, so this
/// is rebuilt on demand rather than cached on the track.
pub fn track_input_detail(
    registry: &AudioConnectionRegistry,
    ports: &AvailablePorts,
    id: &AudioConnectionId,
) -> Option<String> {
    let connection = registry.get(id)?;
    let layout = match connection.channel_layout {
        ChannelLayout::Mono => "Mono".to_string(),
        ChannelLayout::Stereo => "Stereo".to_string(),
        ChannelLayout::Custom { channels } => format!("{channels} ch"),
    };
    let device = registry
        .device_display_name(id, ports)
        .unwrap_or_else(|| "No Device".to_string());
    Some(format!(
        "{layout} · {device} · {}",
        connection.port_summary()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::{AudioConnection, AudioConnectionStatus};

    fn device() -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "Studio 24c", 4, 4)
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

    #[test]
    fn a_track_input_selector_lists_only_input_connections() {
        let registry = registry();
        let options = track_input_options(&registry, 1, None);

        assert_eq!(options.first(), Some(&InputChoice::NoInput));
        assert_eq!(options.last(), Some(&InputChoice::OpenAudioConnections));
        for id in options.iter().filter_map(InputChoice::connection_id) {
            assert_eq!(
                registry.get(id).unwrap().direction,
                AudioConnectionDirection::Input,
                "an output bus must never be offered as a track input"
            );
        }
    }

    /// The list is built from logical buses, so no device or channel string can
    /// reach it: creating hardware mappings is Audio Connections' job.
    #[test]
    fn no_physical_device_is_enumerated_in_the_selector() {
        let registry = registry();
        let labels: Vec<String> = track_input_options(&registry, 2, None)
            .iter()
            .map(InputChoice::label)
            .collect();
        assert!(
            !labels.iter().any(|label| label.contains("Studio 24c")),
            "device names belong in Audio Connections, not the track selector: {labels:?}"
        );
        assert!(labels
            .iter()
            .any(|label| label == "Open Audio Connections..."));
    }

    #[test]
    fn a_stereo_track_sees_stereo_first_then_legitimate_mono_upmixes() {
        let registry = registry();
        let options = track_input_options(&registry, 2, None);
        let names: Vec<String> = options
            .iter()
            .filter_map(InputChoice::connection_id)
            .map(|id| registry.name_of(id).unwrap().to_string())
            .collect();
        assert_eq!(names.first().map(String::as_str), Some("Stereo Input 1-2"));
        assert!(names.iter().any(|name| name == "Mono Input 1"));
    }

    #[test]
    fn entries_carry_a_group_for_selectors_that_want_headings() {
        let registry = registry();
        let options = track_input_options(&registry, 2, None);
        let mono = options
            .iter()
            .find(|choice| {
                choice
                    .connection_id()
                    .is_some_and(|id| registry.name_of(id) == Some("Mono Input 1"))
            })
            .unwrap();
        assert_eq!(mono.group(), Some("Mono"));
        assert_eq!(InputChoice::NoInput.group(), None);
    }

    #[test]
    fn a_disabled_input_is_not_offered_but_a_current_assignment_stays_visible() {
        let ports = device();
        let mut registry = registry();
        let mic = input_id(&registry, "Mono Input 1");
        registry.set_enabled(&mic, false);
        registry.revalidate(&ports);

        assert!(track_input_options(&registry, 1, None)
            .iter()
            .filter_map(InputChoice::connection_id)
            .all(|id| *id != mic));

        let entry = track_input_options(&registry, 1, Some(&mic))
            .into_iter()
            .find(|choice| choice.connection_id() == Some(&mic))
            .expect("the current assignment is always visible");
        assert_eq!(entry.label(), "Mono Input 1 — Unavailable");
    }

    /// Disconnecting the interface must not clear the track's routing.
    #[test]
    fn a_missing_device_is_labelled_unavailable_without_clearing_the_assignment() {
        let mut registry = registry();
        let mic = input_id(&registry, "Mono Input 1");
        registry.revalidate(&AvailablePorts::default());

        assert_eq!(
            registry.get(&mic).unwrap().status,
            AudioConnectionStatus::DeviceMissing
        );
        assert_eq!(
            track_input_label(&registry, Some(&mic)),
            "Mono Input 1 — Unavailable"
        );
        assert_eq!(
            track_input_options(&registry, 1, Some(&mic))
                .iter()
                .filter(|choice| choice.connection_id() == Some(&mic))
                .count(),
            1,
            "the assignment stays selectable exactly once"
        );
    }

    #[test]
    fn no_input_is_the_label_for_an_unassigned_track() {
        let registry = registry();
        assert_eq!(track_input_label(&registry, None), NO_INPUT_LABEL);
    }

    /// An id from another project resolves to nothing here; the selector says so
    /// rather than quietly reporting No Input.
    #[test]
    fn an_unresolvable_id_is_reported_as_missing_not_as_no_input() {
        let registry = registry();
        let ghost = AudioConnectionId::from_stored("ac-ghost");
        assert_eq!(
            track_input_label(&registry, Some(&ghost)),
            MISSING_CONNECTION_LABEL
        );
    }

    #[test]
    fn the_detail_line_names_layout_device_and_ports() {
        let ports = device();
        let registry = registry();
        let stereo = input_id(&registry, "Stereo Input 1-2");
        assert_eq!(
            track_input_detail(&registry, &ports, &stereo).as_deref(),
            Some("Stereo · Studio 24c · Input 1 / Input 2")
        );
    }

    #[test]
    fn renaming_a_connection_changes_the_label_only() {
        let ports = device();
        let mut registry = registry();
        let mic = input_id(&registry, "Mono Input 1");
        registry.rename(&mic, "Vocal Mic");
        registry.revalidate(&ports);

        assert_eq!(track_input_label(&registry, Some(&mic)), "Vocal Mic");
        assert_eq!(input_id(&registry, "Vocal Mic"), mic);
    }

    /// Issue #394: a track's output bound as an input bus in Audio
    /// Connections (here the WDW return a Rodhareist wet/dry/wet split
    /// creates) is offered — usable and selectable — on an ordinary audio
    /// track, and refused, with the reason, on the tracks that feed it.
    mod loopback {
        use super::super::*;
        use crate::audio_connections::{AudioConnection, AudioConnectionStatus, AvailablePort};
        use crate::components::timeline::timeline_state::{
            CreateTrackOptions, InputMonitorMode, SendSlotState, TimelineState, TrackOutputRouting,
            TrackType,
        };

        fn add(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
            state.create_track(CreateTrackOptions {
                track_type,
                name: name.to_string(),
                color: gpui::Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            })
        }

        fn track_mut<'a>(
            state: &'a mut TimelineState,
            id: &str,
        ) -> &'a mut crate::components::timeline::timeline_state::TrackState {
            state
                .tracks
                .iter_mut()
                .find(|track| track.id == id)
                .expect("track")
        }

        struct Fixture {
            state: TimelineState,
            registry: AudioConnectionRegistry,
            bus: AudioConnectionId,
            guitar: String,
            wdw: String,
            bounce: String,
        }

        /// Guitar sends to the WDW return; Bounce is a fresh audio track; one
        /// stereo input bus is bound to the WDW return's L/R.
        fn fixture() -> Fixture {
            let mut state = TimelineState::default();
            state.tracks.clear();
            let guitar = add(&mut state, TrackType::Audio, "Guitar");
            let wdw = add(&mut state, TrackType::Return, "WDW");
            let bounce = add(&mut state, TrackType::Audio, "Bounce");
            track_mut(&mut state, &guitar).sends.push(SendSlotState {
                id: "send-wdw".to_string(),
                target_track_id: wdw.clone(),
                target_name: "WDW".to_string(),
                enabled: true,
                pre_fader: false,
                gain_db: 0.0,
                wdw: true,
                automated: None,
            });

            let device_id = DirectAudio::loopback::loopback_device_id(&wdw);
            let ports = AvailablePorts {
                ports: ["L", "R"]
                    .iter()
                    .enumerate()
                    .map(|(index, name)| AvailablePort {
                        device_id: device_id.clone(),
                        device_name: "WDW".to_string(),
                        port_name: name.to_string(),
                        port_index: index as u32,
                        direction: AudioConnectionDirection::Input,
                    })
                    .collect(),
                ..AvailablePorts::default()
            };
            let mut registry = AudioConnectionRegistry::new();
            let bus = registry.add(
                AudioConnection::new(
                    "WDW In",
                    AudioConnectionDirection::Input,
                    ChannelLayout::Stereo,
                )
                .bind_consecutive(device_id, 0, |index| {
                    if index == 0 { "L" } else { "R" }.to_string()
                }),
            );
            registry.revalidate(&ports);
            assert_eq!(
                registry.get(&bus).unwrap().status,
                AudioConnectionStatus::Active
            );
            Fixture {
                state,
                registry,
                bus,
                guitar,
                wdw,
                bounce,
            }
        }

        fn options_for(fixture: &Fixture, track_id: &str, channels: usize) -> Vec<InputChoice> {
            track_input_options_for(&fixture.registry, channels, None, |connection| {
                loopback_refusal(
                    &fixture.state.tracks,
                    &fixture.registry,
                    track_id,
                    connection,
                )
            })
        }

        fn entry(options: &[InputChoice], bus: &AudioConnectionId) -> InputChoice {
            options
                .iter()
                .find(|choice| choice.connection_id() == Some(bus))
                .cloned()
                .expect("the loopback bus is listed")
        }

        #[test]
        fn a_return_loopback_bus_is_selectable_on_an_audio_track() {
            let fixture = fixture();
            let choice = entry(&options_for(&fixture, &fixture.bounce, 2), &fixture.bus);
            assert_eq!(choice.label(), "WDW In");
            assert_eq!(
                choice.selection(),
                TrackInputSelection::Connection(fixture.bus.clone())
            );
        }

        #[test]
        fn a_track_that_feeds_the_return_is_told_why_it_cannot_take_it() {
            let fixture = fixture();
            let choice = entry(&options_for(&fixture, &fixture.guitar, 2), &fixture.bus);
            assert_eq!(choice.label(), "WDW In — Feedback");
            match choice.selection() {
                TrackInputSelection::Refused(detail) => {
                    assert!(
                        detail.contains("Guitar") && detail.contains("WDW"),
                        "{detail}"
                    );
                }
                other => panic!("a feedback route was selectable: {other:?}"),
            }
        }

        /// Feeding the return through the track's output is the same loop as
        /// through a send.
        #[test]
        fn routing_a_track_output_into_the_return_also_refuses_it() {
            let mut fixture = fixture();
            let wdw = fixture.wdw.clone();
            let bounce = fixture.bounce.clone();
            track_mut(&mut fixture.state, &bounce).routing.output =
                TrackOutputRouting::Bus { bus_id: wdw };
            let choice = entry(&options_for(&fixture, &fixture.bounce, 2), &fixture.bus);
            assert!(matches!(
                choice.selection(),
                TrackInputSelection::Refused(_)
            ));
        }

        /// A mono track does not take a stereo bus — and says so, rather than
        /// the bus quietly missing from that one track's menu.
        #[test]
        fn a_mono_track_lists_the_stereo_bus_with_the_reason() {
            let fixture = fixture();
            let choice = entry(&options_for(&fixture, &fixture.bounce, 1), &fixture.bus);
            assert_eq!(choice.label(), "WDW In — Stereo, track is Mono");
            assert!(matches!(
                choice.selection(),
                TrackInputSelection::Refused(_)
            ));
        }

        /// The plain list (Add Track) is unchanged: nothing refused, nothing
        /// wider than the track.
        #[test]
        fn the_unscoped_list_neither_refuses_nor_lists_wider_buses() {
            let fixture = fixture();
            assert!(track_input_options(&fixture.registry, 1, None)
                .iter()
                .all(|choice| choice.connection_id() != Some(&fixture.bus)));
        }
    }

    /// A bus is never created as a side effect of picking one.
    #[test]
    fn listing_choices_never_mutates_the_registry() {
        let registry = registry();
        let before = registry.clone();
        let _ = track_input_options(&registry, 1, None);
        let _ = track_input_label(&registry, None);
        assert_eq!(registry, before);
    }
}
