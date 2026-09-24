use gpui::{App, Context};

use crate::components;
use crate::components::timeline::timeline_state::{MidiChannel, TrackMidiInputRouting, TrackType};
use sphere_midi_service::{
    HardwareMidiInputMessage, MidiInputEvent, MidiInputRouteStatus, MidiInputRouter,
    MidiInputSource, MidiInputTarget, VirtualKeyboardEvent,
};

use super::StudioLayout;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct VirtualKeyboardTargetStatus {
    pub target: Option<MidiInputTarget>,
    /// Track that owns the incoming performance for recording. This can differ
    /// from `target.track_id` when a MIDI track feeds an Instrument track.
    capture_track_id: Option<String>,
    pub label: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedMidiInputTarget {
    target: MidiInputTarget,
    capture_track_id: String,
}

impl StudioLayout {
    pub(super) fn update_virtual_keyboard_target_status(&mut self, cx: &mut Context<Self>) {
        let status = self.resolve_virtual_keyboard_target(cx);
        let label = status.label.clone();
        let hint = status.hint.clone();
        let _ = self.virtual_keyboard.update(cx, |panel, cx| {
            panel.set_target_status(label, hint);
            cx.notify();
        });
    }

    /// Release virtual-keyboard notes on a later turn, with **only** the panel
    /// entity leased.
    ///
    /// The panel's event sink re-enters `StudioLayout::update` to route the
    /// NoteOffs. Calling [`Self::release_virtual_keyboard_notes`] directly from
    /// `render` or a command dispatch — where `StudioLayout` is already leased —
    /// double-leases and panics (the multi-window crash). Deferring runs the
    /// flush after the current lease is released, so the sink re-entry is safe.
    pub(super) fn defer_release_virtual_keyboard_notes(&self, cx: &mut Context<Self>) {
        let panel = self.virtual_keyboard.clone();
        cx.defer(move |cx| {
            let _ = panel.update(cx, |panel, cx| panel.release_all(cx));
        });
    }

    /// Panic the virtual-keyboard service (all-notes-off + clear pressed/active)
    /// on a later turn, panel-only. Used when quiescing a session for project
    /// load. Deferred for the same re-entrancy reason as
    /// [`Self::defer_release_virtual_keyboard_notes`].
    pub(super) fn defer_panic_virtual_keyboard(&self, cx: &mut Context<Self>) {
        let panel = self.virtual_keyboard.clone();
        cx.defer(move |cx| {
            let _ = panel.update(cx, |panel, cx| panel.panic(cx));
        });
    }

    /// Unregister a window from the virtual-keyboard service and release any
    /// notes it still held, deferred and panel-only (see
    /// [`Self::defer_release_virtual_keyboard_notes`] for why). Safe for a window
    /// that was never registered. Called on MIDI-editor-popout / project close so
    /// a destroyed window never leaves stuck notes or a stale registration.
    pub(super) fn unregister_virtual_keyboard_window(
        &self,
        window_id: gpui::WindowId,
        cx: &mut Context<Self>,
    ) {
        let panel = self.virtual_keyboard.clone();
        cx.defer(move |cx| {
            let _ = panel.update(cx, |panel, cx| panel.unregister_window(window_id, cx));
        });
    }

    pub(super) fn route_virtual_keyboard_event(
        &mut self,
        event: VirtualKeyboardEvent,
        cx: &App,
    ) -> MidiInputRouteStatus {
        let status = self.resolve_virtual_keyboard_target(cx);
        let capture_track_id = status.capture_track_id.clone();
        let Some(target) = status.target else {
            if virtual_keyboard_debug() {
                eprintln!("[vkbd] route event={event:?} target=none -> NoTarget");
            }
            return MidiInputRouteStatus::NoTarget;
        };
        if virtual_keyboard_debug() {
            eprintln!(
                "[vkbd] route event={event:?} target_track={} instance={:?}",
                target.track_id, target.plugin_instance_id
            );
        }
        self.route_midi_input_event_with_capture(
            MidiInputSource::VirtualKeyboard,
            target,
            MidiInputEvent::from(event),
            capture_track_id.as_deref(),
            None,
            cx,
        )
    }

    /// Preview notes from a UI surface (the Chord Generator) on the track the
    /// virtual keyboard would play, without feeding a recording take.
    /// Returns `false` when no instrument track can play them.
    pub(super) fn audition_preview_notes(&mut self, notes: &[u8], on: bool, cx: &App) -> bool {
        let status = self.resolve_virtual_keyboard_target(cx);
        let Some(target) = status.target else {
            return false;
        };
        for &note in notes {
            let event = if on {
                MidiInputEvent::NoteOn {
                    note,
                    velocity: 88,
                    channel: 0,
                }
            } else {
                MidiInputEvent::NoteOff { note, channel: 0 }
            };
            self.route_midi_input_event_with_capture(
                MidiInputSource::PianoRollPreview,
                target.clone(),
                event,
                None,
                None,
                cx,
            );
        }
        true
    }

    /// Routes a gesture from the Soundfont Player window's keyboard or Test
    /// button. The built-in player is a track instrument with no plugin
    /// instance, so this always lands on the engine's track MIDI preview.
    pub(crate) fn dispatch_soundfont_preview(
        &mut self,
        track_id: &str,
        event: components::soundfont_player_window::SoundfontPlayerPreview,
        cx: &mut Context<Self>,
    ) {
        use components::soundfont_player_window::SoundfontPlayerPreview;

        // The engine builds its player from the project snapshot, so a font or
        // preset chosen moments ago has to reach the graph before the note
        // does. Only forced when something is actually pending.
        if self.audio_bridge.project_dirty {
            self.schedule_audio_project_sync(cx, true, "soundfont_preview");
        }

        let event = match event {
            SoundfontPlayerPreview::NoteOn {
                channel,
                pitch,
                velocity,
            } => MidiInputEvent::NoteOn {
                note: pitch,
                velocity,
                channel,
            },
            SoundfontPlayerPreview::NoteOff { channel, pitch } => MidiInputEvent::NoteOff {
                note: pitch,
                channel,
            },
            SoundfontPlayerPreview::AllNotesOff => MidiInputEvent::AllNotesOff,
        };
        let status = self.route_midi_input_event(
            MidiInputSource::VirtualKeyboard,
            MidiInputTarget {
                track_id: track_id.to_string(),
                plugin_instance_id: None,
            },
            event,
            cx,
        );
        match status {
            MidiInputRouteStatus::Routed => {}
            other => eprintln!("[soundfont-player] preview not routed: {other:?}"),
        }
    }

    pub(super) fn route_midi_input_event(
        &mut self,
        source: MidiInputSource,
        target: MidiInputTarget,
        event: MidiInputEvent,
        cx: &App,
    ) -> MidiInputRouteStatus {
        let capture_track_id = target.track_id.clone();
        self.route_midi_input_event_with_capture(
            source,
            target,
            event,
            Some(&capture_track_id),
            None,
            cx,
        )
    }

    fn route_midi_input_event_with_capture(
        &mut self,
        source: MidiInputSource,
        target: MidiInputTarget,
        event: MidiInputEvent,
        capture_track_id: Option<&str>,
        capture_beat: Option<f32>,
        cx: &App,
    ) -> MidiInputRouteStatus {
        let _source = source;
        if let Some(track_id) = capture_track_id {
            self.capture_midi_record_event_at(track_id, &event, capture_beat, cx);
        }
        let bridge_instance = target.plugin_instance_id.clone();
        let sink_ready = match self
            .plugin_editors
            .bridge_runtime
            .as_ref()
            .map(|rt| rt.try_lock())
        {
            Some(Ok(bridge)) => bridge_instance
                .as_ref()
                .is_some_and(|id| bridge.has_audio_sink(id)),
            Some(Err(_)) => bridge_instance.is_some(),
            None => false,
        };

        if sink_ready {
            let Some(engine) = self.audio_bridge.engine.as_ref() else {
                return MidiInputRouteStatus::EngineUnavailable;
            };
            let instance_id = bridge_instance.unwrap_or_default();
            let result = match event {
                MidiInputEvent::NoteOn {
                    note,
                    velocity,
                    channel,
                } => engine.plugin_preview_note_on(
                    target.track_id.clone(),
                    instance_id,
                    MidiInputRouter::sanitize_channel(channel),
                    MidiInputRouter::sanitize_note(note),
                    MidiInputRouter::sanitize_velocity(velocity),
                ),
                MidiInputEvent::NoteOff { note, channel } => engine.plugin_preview_note_off(
                    target.track_id.clone(),
                    instance_id,
                    MidiInputRouter::sanitize_channel(channel),
                    MidiInputRouter::sanitize_note(note),
                ),
                MidiInputEvent::ControlChange {
                    controller,
                    value,
                    channel,
                } => engine.plugin_preview_control_change(
                    target.track_id.clone(),
                    instance_id,
                    MidiInputRouter::sanitize_channel(channel),
                    controller.min(127),
                    value.min(127),
                ),
                MidiInputEvent::PitchBend { value, channel } => engine.plugin_preview_pitch_bend(
                    target.track_id.clone(),
                    instance_id,
                    MidiInputRouter::sanitize_channel(channel),
                    value.min(16_383),
                ),
                MidiInputEvent::ChannelPressure { value, channel } => engine
                    .plugin_preview_control_change(
                        target.track_id.clone(),
                        instance_id,
                        MidiInputRouter::sanitize_channel(channel),
                        128,
                        value.min(127),
                    ),
                // Poly pressure is decoded and preserved by the MPE layer;
                // this legacy preview bridge has no per-note API yet.
                MidiInputEvent::PolyPressure { .. } => Ok(()),
                MidiInputEvent::AllNotesOff | MidiInputEvent::Panic => {
                    engine.plugin_preview_all_notes_off(target.track_id.clone(), instance_id)
                }
            };
            return route_result(result);
        }

        if let Some(instance_id) = bridge_instance.clone() {
            if let Some(runtime) = self.plugin_editors.bridge_runtime.as_ref() {
                if let Ok(mut bridge) = runtime.try_lock() {
                    let result = match event {
                        MidiInputEvent::NoteOn {
                            note,
                            velocity,
                            channel,
                        } => bridge.preview_note_on(
                            instance_id,
                            MidiInputRouter::sanitize_channel(channel),
                            MidiInputRouter::sanitize_note(note),
                            MidiInputRouter::sanitize_velocity(velocity),
                        ),
                        MidiInputEvent::NoteOff { note, channel } => bridge.preview_note_off(
                            instance_id,
                            MidiInputRouter::sanitize_channel(channel),
                            MidiInputRouter::sanitize_note(note),
                        ),
                        MidiInputEvent::ControlChange {
                            controller,
                            value,
                            channel,
                        } => bridge.preview_control_change(
                            instance_id,
                            MidiInputRouter::sanitize_channel(channel),
                            controller.min(127),
                            value.min(127),
                        ),
                        MidiInputEvent::PitchBend { value, channel } => bridge
                            .preview_control_change(
                                instance_id,
                                MidiInputRouter::sanitize_channel(channel),
                                129,
                                (value.min(16_383) >> 7) as u8,
                            ),
                        MidiInputEvent::ChannelPressure { value, channel } => bridge
                            .preview_control_change(
                                instance_id,
                                MidiInputRouter::sanitize_channel(channel),
                                128,
                                value.min(127),
                            ),
                        MidiInputEvent::PolyPressure { .. } => Ok(()),
                        MidiInputEvent::AllNotesOff => bridge.preview_all_notes_off(instance_id),
                        MidiInputEvent::Panic => bridge.midi_panic(instance_id),
                    };
                    return route_result(result);
                }
            }
        }

        let Some(engine) = self.audio_bridge.engine.as_ref() else {
            return MidiInputRouteStatus::EngineUnavailable;
        };
        let result = match event {
            MidiInputEvent::NoteOn {
                note,
                velocity,
                channel,
            } => engine.midi_preview_note_on(
                target.track_id.clone(),
                MidiInputRouter::sanitize_channel(channel),
                MidiInputRouter::sanitize_note(note),
                MidiInputRouter::sanitize_velocity(velocity),
            ),
            MidiInputEvent::NoteOff { note, channel } => engine.midi_preview_note_off(
                target.track_id.clone(),
                MidiInputRouter::sanitize_channel(channel),
                MidiInputRouter::sanitize_note(note),
            ),
            MidiInputEvent::ControlChange {
                controller,
                value,
                channel,
            } => engine.midi_preview_control_change(
                target.track_id.clone(),
                MidiInputRouter::sanitize_channel(channel),
                controller.min(127),
                value.min(127),
            ),
            MidiInputEvent::PitchBend { value, channel } => engine.midi_preview_pitch_bend(
                target.track_id.clone(),
                MidiInputRouter::sanitize_channel(channel),
                value.min(16_383),
            ),
            MidiInputEvent::ChannelPressure { value, channel } => engine
                .midi_preview_control_change(
                    target.track_id.clone(),
                    MidiInputRouter::sanitize_channel(channel),
                    128,
                    value.min(127),
                ),
            MidiInputEvent::PolyPressure { .. } => Ok(()),
            MidiInputEvent::AllNotesOff | MidiInputEvent::Panic => {
                engine.midi_preview_all_notes_off(target.track_id.clone())
            }
        };
        route_result(result)
    }

    /// Sync enabled hardware MIDI input ports and route pending messages into
    /// armed/selected instrument tracks (and the active MIDI recording take).
    pub(super) fn sync_hardware_midi_input_devices(&mut self, cx: &mut Context<Self>) {
        let devices = {
            let settings = self.settings.read(cx);
            settings.current.hardware.midi.devices.clone()
        };
        // Re-resolve against the live scan so connected flags stay current even
        // when Preferences is closed.
        let detected = crate::device_registry::cached_midi_devices();
        let resolved = sphere_midi_service::resolve_midi_devices(&devices, &detected);
        self.hardware_midi_input.sync_enabled_devices(&resolved);
        log_hardware_midi_diagnostics();
    }

    pub(super) fn drain_hardware_midi_input(&mut self, cx: &mut Context<Self>) -> bool {
        let messages = self.hardware_midi_input.drain(64);
        if messages.is_empty() {
            return false;
        }
        for message in messages {
            self.route_hardware_midi_message(message, cx);
        }
        true
    }

    fn route_hardware_midi_message(
        &mut self,
        message: HardwareMidiInputMessage,
        cx: &mut Context<Self>,
    ) {
        // Compensate for control-thread scheduling delay. Screen capture can
        // delay GPUI work even though the native MIDI callback arrived on time;
        // recording placement follows that arrival rather than the later drain.
        let capture_beat = self.audio_bridge.engine.as_ref().map(|engine| {
            let snapshot = engine.transport_snapshot();
            let delay = message.received_at.elapsed().as_secs_f64();
            // Back the arrival out in *seconds* and resolve the beat through
            // the tempo map. `beat - delay * bpm / 60` assumes one tempo for
            // the whole project, so a note played just after a tempo change
            // used to land at the wrong distance from it — by more the bigger
            // the change.
            let state = &self.timeline.read(cx).state;
            compensated_midi_capture_beat(snapshot.position_seconds, delay, |seconds| {
                state.beat_at_seconds(seconds)
            })
        });
        let channel = match &message.event {
            MidiInputEvent::NoteOn { channel, .. }
            | MidiInputEvent::NoteOff { channel, .. }
            | MidiInputEvent::ControlChange { channel, .. }
            | MidiInputEvent::PitchBend { channel, .. }
            | MidiInputEvent::ChannelPressure { channel, .. }
            | MidiInputEvent::PolyPressure { channel, .. } => Some(*channel),
            MidiInputEvent::AllNotesOff | MidiInputEvent::Panic => None,
        };
        let targets = self.resolve_hardware_midi_targets(
            &message.device_id,
            &message.device_name,
            channel,
            cx,
        );
        sphere_midi_service::note_midi_input_routed(!targets.is_empty());
        if targets.is_empty() {
            if hardware_midi_debug() {
                eprintln!(
                    "[MIDI input] {} ({}) event={:?} channel={channel:?} -> no target \
                     (no armed/selected instrument track accepts this device)",
                    message.device_name, message.device_id, message.event
                );
            }
            return;
        }
        for resolved in targets {
            let status = self.route_midi_input_event_with_capture(
                MidiInputSource::Hardware,
                resolved.target.clone(),
                message.event.clone(),
                Some(&resolved.capture_track_id),
                capture_beat,
                cx,
            );
            if hardware_midi_debug() {
                eprintln!(
                    "[MIDI input] {} ({}) event={:?} -> track={} instance={:?} status={status:?}",
                    message.device_name,
                    message.device_id,
                    message.event,
                    resolved.target.track_id,
                    resolved.target.plugin_instance_id
                );
            }
        }
    }

    fn resolve_hardware_midi_targets(
        &self,
        device_id: &str,
        device_name: &str,
        channel: Option<u8>,
        cx: &App,
    ) -> Vec<ResolvedMidiInputTarget> {
        let timeline = self.timeline.read(cx);
        let state = &timeline.state;
        let mut targets = Vec::new();

        for track in &state.tracks {
            if !is_keyboard_target_candidate(track) {
                continue;
            }
            if !track_accepts_hardware_midi(&track.routing.midi_input, device_id, device_name) {
                continue;
            }
            if let Some(ch) = channel {
                let midi_ch = MidiChannel::from_raw(ch);
                if !track.routing.midi_input_filter.accepts(midi_ch) {
                    continue;
                }
            }
            // Prefer armed tracks; also accept the selected track so live play
            // works without arming (same as the virtual keyboard).
            let selected = state.selection.selected_track_id.as_deref() == Some(track.id.as_str());
            if !track.armed && !selected {
                continue;
            }
            if let Some(target) = hardware_midi_target_for_track(state, track) {
                targets.push(target);
            }
        }

        // If nothing matched via arm/selection but a single AllInputs armed
        // path exists above, we're done. If still empty, fall back to the
        // virtual-keyboard resolver so an instrument track with default
        // AllInputs still receives notes when selected.
        if targets.is_empty() {
            let fallback = self.resolve_virtual_keyboard_target(cx);
            if let (Some(target), Some(capture_track_id)) =
                (fallback.target, fallback.capture_track_id)
            {
                // Re-check the fallback track still accepts this device.
                if let Some(track) = state.find_track(&capture_track_id) {
                    if track_accepts_hardware_midi(
                        &track.routing.midi_input,
                        device_id,
                        device_name,
                    ) {
                        targets.push(ResolvedMidiInputTarget {
                            target,
                            capture_track_id,
                        });
                    }
                }
            }
        }
        targets
    }

    pub(super) fn resolve_virtual_keyboard_target(&self, cx: &App) -> VirtualKeyboardTargetStatus {
        let timeline = self.timeline.read(cx);
        let state = &timeline.state;

        let selected = state
            .selection
            .selected_track_id
            .as_deref()
            .and_then(|track_id| state.find_track(track_id))
            .filter(|track| is_keyboard_target_candidate(track));

        let track = selected.or_else(|| {
            state
                .tracks
                .iter()
                .find(|track| track.armed && is_keyboard_target_candidate(track))
        });

        let Some(track) = track else {
            return VirtualKeyboardTargetStatus {
                target: None,
                capture_track_id: None,
                label: None,
                hint: Some("Select or arm an instrument track to play.".to_string()),
            };
        };

        let destination = state
            .effective_instrument_track_id(&track.id)
            .and_then(|track_id| state.find_track(&track_id));
        let plugin_instance_id = destination.and_then(instrument_instance_id);

        let Some(plugin_instance_id) = plugin_instance_id else {
            if destination.is_some_and(|destination| destination.solfege.is_some()) {
                let destination = destination.expect("checked above");
                return VirtualKeyboardTargetStatus {
                    target: Some(MidiInputTarget {
                        track_id: destination.id.clone(),
                        plugin_instance_id: None,
                    }),
                    capture_track_id: Some(track.id.clone()),
                    label: Some(track.name.clone()),
                    hint: None,
                };
            }
            // The built-in Soundfont Player is a track instrument rather than a
            // hosted plugin, so it has no instance id — the engine's track MIDI
            // preview reaches it directly.
            if destination.is_some_and(|destination| destination.builtin_soundfont_player) {
                let destination = destination.expect("checked above");
                let loaded = destination
                    .soundfont_path
                    .as_deref()
                    .is_some_and(|path| !path.is_empty());
                return VirtualKeyboardTargetStatus {
                    target: loaded.then(|| MidiInputTarget {
                        track_id: destination.id.clone(),
                        plugin_instance_id: None,
                    }),
                    capture_track_id: loaded.then(|| track.id.clone()),
                    label: Some(track.name.clone()),
                    hint: (!loaded).then(|| "Load a SoundFont on the selected track.".to_string()),
                };
            }
            return VirtualKeyboardTargetStatus {
                target: None,
                capture_track_id: None,
                label: Some(track.name.clone()),
                hint: Some("Load an instrument on the selected track.".to_string()),
            };
        };

        VirtualKeyboardTargetStatus {
            target: Some(MidiInputTarget {
                track_id: destination
                    .map(|destination| destination.id.clone())
                    .unwrap_or_else(|| track.id.clone()),
                plugin_instance_id: Some(plugin_instance_id),
            }),
            capture_track_id: Some(track.id.clone()),
            label: Some(track.name.clone()),
            hint: None,
        }
    }
}

/// `FUTUREBOARD_VIRTUAL_KEYBOARD_DEBUG=1` also traces routed virtual-keyboard
/// events here (event + resolved target track), complementing the per-key
/// classification trace in `virtual_keyboard.rs`. Cached on first read.
fn virtual_keyboard_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_VIRTUAL_KEYBOARD_DEBUG").is_some())
}

/// `FUTUREBOARD_MIDI_INPUT_DEBUG=1` traces each drained hardware MIDI event
/// through routing: the device it came from, the target track and plugin
/// instance it landed on, and the dispatch status — or why nothing claimed it.
///
/// Deliberately opt-in and off the MIDI callback thread: this runs on the
/// control-thread drain, so a held key cannot turn into per-event stdout work
/// on CoreMIDI's high-priority thread. Cached on first read.
fn hardware_midi_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_MIDI_INPUT_DEBUG").is_some())
}

/// Print the hardware MIDI input counters when they move, from the 500 ms
/// device sync (never from the MIDI callback).
///
/// These are what separate the failure modes that otherwise all look like
/// "nothing happens when I press a key": `callbacks=0` means the backend never
/// delivered anything, `messages>0 events=0` means bytes arrived but nothing
/// decoded, and `events>0 routed=0` means decoding worked but no track claimed
/// the event.
fn log_hardware_midi_diagnostics() {
    if !hardware_midi_debug() {
        return;
    }
    use std::cell::Cell;
    thread_local! {
        static LAST: Cell<sphere_midi_service::MidiInputDiagnostics> =
            const { Cell::new(sphere_midi_service::MidiInputDiagnostics {
                callbacks: 0, messages: 0, events: 0, ignored: 0,
                malformed: 0, dropped: 0, routed: 0, unrouted: 0,
            }) };
    }
    let now = sphere_midi_service::midi_input_diagnostics();
    LAST.with(|last| {
        if last.get() == now {
            return;
        }
        last.set(now);
        eprintln!(
            "[MIDI input] callbacks={} messages={} events={} ignored={} malformed={} \
             dropped={} routed={} unrouted={}",
            now.callbacks,
            now.messages,
            now.events,
            now.ignored,
            now.malformed,
            now.dropped,
            now.routed,
            now.unrouted
        );
    });
}

fn route_result<E: std::fmt::Display>(result: Result<(), E>) -> MidiInputRouteStatus {
    match result {
        Ok(()) => MidiInputRouteStatus::Routed,
        Err(error) => MidiInputRouteStatus::DispatchFailed(error.to_string()),
    }
}

fn is_keyboard_target_candidate(track: &components::timeline::timeline_state::TrackState) -> bool {
    matches!(track.track_type, TrackType::Instrument | TrackType::Midi)
}

fn track_accepts_hardware_midi(
    routing: &TrackMidiInputRouting,
    device_id: &str,
    device_name: &str,
) -> bool {
    match routing {
        TrackMidiInputRouting::None => false,
        TrackMidiInputRouting::AllInputs => true,
        TrackMidiInputRouting::MidiDevice {
            device_id: route_id,
        } => route_id == device_id || route_id == device_name,
    }
}

fn hardware_midi_target_for_track(
    state: &components::timeline::timeline_state::TimelineState,
    track: &components::timeline::timeline_state::TrackState,
) -> Option<ResolvedMidiInputTarget> {
    let destination = state
        .effective_instrument_track_id(&track.id)
        .and_then(|track_id| state.find_track(&track_id));
    let plugin_instance_id = destination.and_then(instrument_instance_id);
    if plugin_instance_id.is_some() {
        return Some(ResolvedMidiInputTarget {
            target: MidiInputTarget {
                track_id: destination?.id.clone(),
                plugin_instance_id,
            },
            capture_track_id: track.id.clone(),
        });
    }
    if let Some(destination) = destination.filter(|destination| {
        destination.builtin_soundfont_player
            && destination
                .soundfont_path
                .as_deref()
                .is_some_and(|path| !path.is_empty())
    }) {
        return Some(ResolvedMidiInputTarget {
            target: MidiInputTarget {
                track_id: destination.id.clone(),
                plugin_instance_id: None,
            },
            capture_track_id: track.id.clone(),
        });
    }
    if let Some(destination) = destination.filter(|destination| destination.solfege.is_some()) {
        return Some(ResolvedMidiInputTarget {
            target: MidiInputTarget {
                track_id: destination.id.clone(),
                plugin_instance_id: None,
            },
            capture_track_id: track.id.clone(),
        });
    }
    // MIDI tracks without an instrument still accept input while recording
    // (notes land in the take buffer via capture_midi_record_event).
    if track.track_type == TrackType::Midi {
        return Some(ResolvedMidiInputTarget {
            target: MidiInputTarget {
                track_id: track.id.clone(),
                plugin_instance_id: None,
            },
            capture_track_id: track.id.clone(),
        });
    }
    None
}

fn instrument_instance_id(
    track: &components::timeline::timeline_state::TrackState,
) -> Option<String> {
    track
        .instrument_plugin_instance_id
        .clone()
        .or_else(|| first_instrument_insert_id(track))
}

fn first_instrument_insert_id(
    track: &components::timeline::timeline_state::TrackState,
) -> Option<String> {
    match track.track_type {
        TrackType::Instrument => track
            .instrument_insert()
            .filter(|insert| insert.plugin_id.is_some())
            .map(|insert| insert.id.clone()),
        TrackType::Midi => track
            .inserts
            .first()
            .filter(|insert| insert.plugin_id.is_some())
            .map(|insert| insert.id.clone()),
        _ => None,
    }
}

/// Where a MIDI event that arrived `queue_delay_seconds` ago belongs.
///
/// The compensation is done on the clock and the *result* converted to a beat,
/// rather than converted first and subtracted in beats: seconds are what the
/// delay is measured in, and beats are only linear in seconds while the tempo
/// is constant.
fn compensated_midi_capture_beat(
    position_seconds: f64,
    queue_delay_seconds: f64,
    beat_at_seconds: impl Fn(f64) -> f64,
) -> f32 {
    let seconds = (position_seconds - queue_delay_seconds.max(0.0)).max(0.0);
    beat_at_seconds(seconds).max(0.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, TimelineState, TrackOutputRouting,
    };

    fn add_track(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 0.8,
            pan: 0.0,
            armed: true,
            input_monitor: InputMonitorMode::WhenRecordArmed,
        })
    }

    #[test]
    fn midi_record_timing_compensates_control_queue_delay() {
        // 120 BPM: half a second per beat.
        let at_120 = |seconds: f64| seconds * 2.0;
        // Beat 8 is 4.0 s in; an event that arrived 25 ms ago belongs at 7.95.
        let beat = compensated_midi_capture_beat(4.0, 0.025, at_120);
        assert!((beat - 7.95).abs() < 0.0001, "got {beat}");
        // A delay longer than the take clamps to the start rather than going
        // negative.
        assert_eq!(compensated_midi_capture_beat(0.005, 1.0, at_120), 0.0);
    }

    /// The delay is a wall-clock quantity, so it has to be taken off the clock
    /// and the beat resolved afterwards. Subtracting `delay * bpm / 60` — one
    /// tempo for the whole project — puts a note played just after a tempo
    /// change at the wrong distance from it, by more the larger the change.
    #[test]
    fn midi_record_timing_follows_the_tempo_map_across_a_change() {
        // 60 BPM (1 s/beat) for the first 4 beats, then 240 BPM (0.25 s/beat).
        let map = |seconds: f64| {
            if seconds <= 4.0 {
                seconds
            } else {
                4.0 + (seconds - 4.0) * 4.0
            }
        };
        // 40 ms into the fast section, an event that arrived 80 ms ago was
        // played 40 ms *before* the change — in the slow section.
        let beat = compensated_midi_capture_beat(4.04, 0.08, map);
        assert!((beat - 3.96).abs() < 0.0001, "got {beat}");
        // Whereas one that arrived 20 ms ago is already past it.
        let beat = compensated_midi_capture_beat(4.04, 0.02, map);
        assert!((beat - 4.08).abs() < 0.0001, "got {beat}");
    }

    #[test]
    fn routed_midi_input_targets_destination_vsti_and_keeps_source_for_capture() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let midi_id = add_track(&mut state, TrackType::Midi, "MIDI");
        let instrument_id = add_track(&mut state, TrackType::Instrument, "Synth");
        let instance_id = "insert-synth-1".to_string();
        state
            .tracks
            .iter_mut()
            .find(|track| track.id == instrument_id)
            .unwrap()
            .instrument_plugin_instance_id = Some(instance_id.clone());
        assert!(state.set_track_output_routing(
            &midi_id,
            TrackOutputRouting::Instrument {
                track_id: instrument_id.clone(),
            },
        ));

        let source = state.find_track(&midi_id).unwrap();
        let resolved = hardware_midi_target_for_track(&state, source).unwrap();
        assert_eq!(resolved.capture_track_id, midi_id);
        assert_eq!(resolved.target.track_id, instrument_id);
        assert_eq!(resolved.target.plugin_instance_id, Some(instance_id));
    }
}
