//! RuntimeProject: MIDI and bridged-insert preview notes.

use super::*;

impl RuntimeProject {
    pub fn midi_preview_note_on(&mut self, track_id: &str, channel: u8, pitch: u8, velocity: u8) {
        self.bridge_preview_note_on(track_id, "", channel, pitch, velocity);
    }

    pub fn midi_preview_note_off(&mut self, track_id: &str, channel: u8, pitch: u8) {
        self.bridge_preview_note_off(track_id, "", channel, pitch);
    }

    pub fn midi_preview_control_change(
        &mut self,
        track_id: &str,
        channel: u8,
        controller: u8,
        value: u8,
    ) {
        self.bridge_preview_control_change(track_id, "", channel, controller, value);
    }

    pub fn midi_preview_all_notes_off(&mut self, track_id: &str) {
        self.bridge_preview_all_notes_off(track_id, "");
    }

    /// Push a preview note-on on the audio thread. When a bridge sink is
    /// installed, writes directly into the shared MIDI ring (sample_offset=0).
    pub fn bridge_preview_note_on(
        &mut self,
        track_id: &str,
        plugin_instance_id: &str,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) {
        let channel = channel.min(15);
        let pitch = pitch.min(127);
        let velocity = velocity.clamp(1, 127);
        let bridged = self.plugin_bridge_sinks.contains_key(plugin_instance_id);
        if bridged {
            // Shared-memory path: always write into the realtime MIDI ring on the
            // audio thread. Do not rely on midi_block_events / runtime inserts.
            self.push_bridge_preview_midi(
                plugin_instance_id,
                0x90 | channel,
                pitch,
                velocity,
                "note_on",
            );
            self.set_preview_active(track_id, channel, pitch, true);
            return;
        }
        if self.queue_preview_event(
            track_id,
            Vst3MidiEvent::note_on(0, channel, pitch, velocity as f32 / 127.0),
            "note_on",
            channel,
            pitch,
        ) {
            self.set_preview_active(track_id, channel, pitch, true);
        }
    }

    pub fn bridge_preview_note_off(
        &mut self,
        track_id: &str,
        plugin_instance_id: &str,
        channel: u8,
        pitch: u8,
    ) {
        let channel = channel.min(15);
        let pitch = pitch.min(127);
        if crate::forensic_trace::engine_midi_verbose_enabled() {
            eprintln!(
                "[EngineMidiPreview] received note_off track={} instance={} ch={} pitch={}",
                track_id, plugin_instance_id, channel, pitch
            );
        }
        let bridged = self.plugin_bridge_sinks.contains_key(plugin_instance_id);
        if bridged {
            self.push_bridge_preview_midi(plugin_instance_id, 0x80 | channel, pitch, 0, "note_off");
            self.set_preview_active(track_id, channel, pitch, false);
            return;
        }
        if self.queue_preview_event(
            track_id,
            Vst3MidiEvent::note_off(0, channel, pitch, 0.0),
            "note_off",
            channel,
            pitch,
        ) {
            self.set_preview_active(track_id, channel, pitch, false);
        }
    }

    pub fn bridge_preview_control_change(
        &mut self,
        track_id: &str,
        plugin_instance_id: &str,
        channel: u8,
        controller: u8,
        value: u8,
    ) {
        let channel = channel.min(15);
        let value = value.min(127);
        // `128`/`129` are the VST3 out-of-band controller numbers for channel
        // pressure and pitch bend. Clamping them to 127 turned every bend into
        // CC 127 (Poly Mode On), so they keep their own meaning here.
        if controller == 129 {
            // 7-bit lane value: expand so centre (64) lands on 8192.
            let bend = u16::from(value) << 7;
            self.bridge_preview_pitch_bend(track_id, plugin_instance_id, channel, bend);
            return;
        }
        let controller = controller.min(128);
        if self.plugin_bridge_sinks.contains_key(plugin_instance_id) {
            if controller == 128 {
                self.push_bridge_preview_midi(
                    plugin_instance_id,
                    0xD0 | channel,
                    value,
                    0,
                    "channel_pressure",
                );
            } else {
                self.push_bridge_preview_midi(
                    plugin_instance_id,
                    0xB0 | channel,
                    controller,
                    value,
                    "control_change",
                );
            }
            return;
        }
        let _ = self.queue_preview_event(
            track_id,
            Vst3MidiEvent::control_change(0, channel, controller as u16, value as f32 / 127.0),
            "control_change",
            channel,
            controller,
        );
    }

    /// Push a 14-bit preview pitch bend (`8192` = centre) on the audio thread.
    /// A bridge sink gets the raw `0xE0` status; an in-process instrument gets
    /// the VST3 `kPitchBend` controller (129) with a normalized value.
    pub fn bridge_preview_pitch_bend(
        &mut self,
        track_id: &str,
        plugin_instance_id: &str,
        channel: u8,
        value: u16,
    ) {
        let channel = channel.min(15);
        let value = value.min(16_383);
        if self.plugin_bridge_sinks.contains_key(plugin_instance_id) {
            self.push_bridge_preview_midi(
                plugin_instance_id,
                0xE0 | channel,
                (value & 0x7F) as u8,
                (value >> 7) as u8,
                "pitch_bend",
            );
            return;
        }
        let _ = self.queue_preview_event(
            track_id,
            Vst3MidiEvent::control_change(0, channel, 129, f32::from(value) / 16_383.0),
            "pitch_bend",
            channel,
            129,
        );
    }

    pub fn bridge_preview_all_notes_off(&mut self, track_id: &str, plugin_instance_id: &str) {
        let (active, track_index) = self
            .midi_tracks
            .iter()
            .find(|mt| mt.track_id == track_id)
            .map(|mt| (mt.preview_active.clone(), mt.track_index))
            .unwrap_or_default();
        // Command path (not per-block): fall back to an id lookup so the panic
        // CCs still reach a track that never had a MIDI schedule entry.
        let track_index = track_index.or_else(|| self.tracks.iter().position(|t| t.id == track_id));
        if crate::forensic_trace::engine_midi_verbose_enabled() {
            eprintln!(
                "[EngineMidiPreview] received all_notes_off track={} instance={} active_notes={}",
                track_id,
                plugin_instance_id,
                active.len()
            );
        }
        push_all_notes_off_for_track(self, track_index, &active, 0, true);
        if self.plugin_bridge_sinks.contains_key(plugin_instance_id) {
            if let Some(sink) = self.plugin_bridge_sinks.get(plugin_instance_id) {
                for &(channel, pitch) in &active {
                    sink.push_midi(0x80 | (channel & 0x0F), pitch, 0, 0);
                }
                for ch in 0u8..16 {
                    sink.push_midi(0xB0 | (ch & 0x0F), 64, 0, 0);
                    sink.push_midi(0xB0 | (ch & 0x0F), 123, 0, 0);
                    sink.push_midi(0xB0 | (ch & 0x0F), 120, 0, 0);
                }
            }
        }
        if let Some(mt) = self
            .midi_tracks
            .iter_mut()
            .find(|mt| mt.track_id == track_id)
        {
            mt.preview_active.clear();
        }
    }

    pub(super) fn push_bridge_preview_midi(
        &self,
        plugin_instance_id: &str,
        status: u8,
        data1: u8,
        data2: u8,
        kind: &str,
    ) {
        let Some(sink) = self.plugin_bridge_sinks.get(plugin_instance_id) else {
            if midi_verbose_enabled() {
                eprintln!(
                    "[plugin-dsp-midi] write skipped instance={plugin_instance_id} reason=no_bridge_sink keys={:?}",
                    self.plugin_bridge_sinks.keys().collect::<Vec<_>>()
                );
            }
            return;
        };
        sink.push_midi(status, data1, data2, 0);
        if crate::forensic_trace::engine_midi_verbose_enabled() {
            let instance = if plugin_instance_id.is_empty() {
                "unknown"
            } else {
                plugin_instance_id
            };
            let seq = MIDI_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[plugin-dsp-midi-write] preview {kind} instance={instance} pitch={data1} offset=0"
            );
            eprintln!("[plugin-dsp-midi-write] seq={seq} instance={instance} events=1");
        }
    }

    pub fn has_active_midi_preview(&self) -> bool {
        self.midi_tracks
            .iter()
            .any(|mt| !mt.preview_active.is_empty())
    }

    pub(super) fn queue_preview_event(
        &mut self,
        track_id: &str,
        event: Vst3MidiEvent,
        event_type: &str,
        channel: u8,
        pitch: u8,
    ) -> bool {
        // Runs on the audio thread per preview event — route diagnostics (with
        // their String formatting) only exist under the verbose trace flag.
        let verbose = crate::forensic_trace::engine_midi_verbose_enabled();
        let Some(ti) = self.tracks.iter().position(|t| t.id == track_id) else {
            if verbose {
                eprintln!(
                    "[InstrumentRoute] track={} no instrument plugin found reason=missing_track",
                    track_id
                );
            }
            return false;
        };
        let track = &self.tracks[ti];
        if verbose {
            let plugins = track
                .inserts
                .iter()
                .map(|insert| {
                    format!(
                        "{}:{}:{}",
                        insert.id,
                        insert.kind,
                        if insert.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            eprintln!(
                "[InstrumentRoute] track={} kind={} plugins={}",
                track.id, track.track_type, plugins
            );
        }
        let Some(insert_ix) = track.midi_instrument_insert_ix else {
            // The built-in Soundfont Player is a track instrument, not an
            // insert, so it has no instrument insert index. Its events are
            // consumed by `render_soundfont_instrument_block` straight from
            // `midi_block_events` — the same queue an instrument insert reads.
            if track.soundfont_player.is_some() || track.solfege_engine.is_some() {
                if verbose {
                    eprintln!(
                        "[InstrumentRoute] track={} selected_instrument_plugin=builtin_native",
                        track_id
                    );
                    eprintln!(
                        "[PluginMidiIn] plugin=builtin_soundfont_player {event_type} ch={channel} pitch={pitch} offset=0"
                    );
                }
                return push_midi_block_event(&mut self.tracks[ti].midi_block_events, event);
            }
            if verbose {
                eprintln!(
                    "[InstrumentRoute] track={} selected_instrument_plugin=none no instrument plugin found",
                    track_id
                );
            }
            return false;
        };
        if verbose {
            let plugin_id = &self.tracks[ti].inserts[insert_ix].id;
            eprintln!(
                "[InstrumentRoute] track={} selected_instrument_plugin={}",
                track_id, plugin_id
            );
            eprintln!(
                "[PluginMidiIn] plugin={} {} ch={} pitch={} offset=0",
                plugin_id, event_type, channel, pitch
            );
            eprintln!(
                "[EngineMidiPreview] target plugin={} event queued",
                plugin_id
            );
        }
        push_midi_block_event(&mut self.tracks[ti].midi_block_events, event)
    }

    pub(super) fn set_preview_active(
        &mut self,
        track_id: &str,
        channel: u8,
        pitch: u8,
        active: bool,
    ) {
        let Some(mt) = self
            .midi_tracks
            .iter_mut()
            .find(|mt| mt.track_id == track_id)
        else {
            let track_index = self.tracks.iter().position(|t| t.id == track_id);
            self.midi_tracks.push(RuntimeMidiTrack {
                track_id: track_id.to_string(),
                track_index,
                events: Vec::new(),
                cursor: 0,
                active: Vec::with_capacity(128),
                preview_active: Vec::with_capacity(128),
                sources: Vec::new(),
                block_events_spare: Vec::new(),
            });
            let Some(mt) = self
                .midi_tracks
                .iter_mut()
                .find(|mt| mt.track_id == track_id)
            else {
                return;
            };
            if active {
                mt.preview_active.push((channel, pitch));
            }
            return;
        };
        let key = (channel, pitch);
        if active {
            if !mt.preview_active.contains(&key) {
                mt.preview_active.push(key);
            }
        } else {
            mt.preview_active.retain(|k| *k != key);
        }
    }
}
