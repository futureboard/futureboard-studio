//! SysEx on MIDI clips and markers: the state half of the SysEx Editor.
//!
//! Clip events keep the Standard MIDI File shape they were imported in
//! (`data` is the bytes after `F0`), so an imported file round-trips byte for
//! byte. Marker messages are stored complete (`F0 … F7`). The conversions
//! between the two live here and in `sphere_midi_service::sysex`, nowhere
//! else.

use super::*;
use sphere_midi_service::sysex;

/// SMF ticks per beat used for the informational `tick` of edited events,
/// matching the exporter.
const EDIT_TICKS_PER_BEAT: f32 = 960.0;

impl MidiSysExEvent {
    /// A normal (`F0`) event from a complete message. `None` unless the
    /// message starts with `F0`.
    pub fn from_message(beat: f32, message: &[u8]) -> Option<Self> {
        let data = sysex::to_smf_payload(message)?;
        let beat = beat.max(0.0);
        Some(Self {
            kind: MidiSysExKind::Normal,
            tick: (beat * EDIT_TICKS_PER_BEAT).round() as u64,
            beat,
            data,
        })
    }

    /// The bytes as they go on the wire: `F0 …` for a normal event, the raw
    /// escape bytes for an `F7` event.
    pub fn message(&self) -> Vec<u8> {
        match self.kind {
            MidiSysExKind::Normal => sysex::from_smf_payload(&self.data),
            MidiSysExKind::Escaped => self.data.clone(),
        }
    }

    /// Move to `beat`, keeping the informational tick in step.
    pub fn at_beat(mut self, beat: f32) -> Self {
        self.beat = beat.max(0.0);
        self.tick = (self.beat * EDIT_TICKS_PER_BEAT).round() as u64;
        self
    }
}

impl TimelineState {
    /// A MIDI clip's SysEx events, in beat order. `None` for a non-MIDI clip.
    pub fn midi_clip_sysex(&self, clip_id: &str) -> Option<&Vec<MidiSysExEvent>> {
        self.find_clip(clip_id)
            .and_then(|(_, clip)| match &clip.clip_type {
                ClipType::Midi { sysex_events, .. } => Some(sysex_events),
                _ => None,
            })
    }

    /// Replace a MIDI clip's SysEx events (undo command payload). Kept sorted
    /// by beat — stable, so messages sharing a beat keep the order they were
    /// written in, which is the order they are sent.
    pub fn set_midi_clip_sysex(&mut self, clip_id: &str, mut events: Vec<MidiSysExEvent>) -> bool {
        events.sort_by(|a, b| a.beat.total_cmp(&b.beat));
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if clip.id != clip_id {
                    continue;
                }
                if let ClipType::Midi { sysex_events, .. } = &mut clip.clip_type {
                    bump_midi_edit_revision();
                    *sysex_events = events;
                    return true;
                }
                return false;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_convert_between_smf_and_wire_form() {
        let gm_on = [0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7];
        let event = MidiSysExEvent::from_message(2.0, &gm_on).unwrap();
        assert_eq!(event.data, vec![0x7E, 0x7F, 0x09, 0x01, 0xF7]);
        assert_eq!(event.tick, 1920);
        assert_eq!(event.message(), gm_on.to_vec());
        assert!(MidiSysExEvent::from_message(0.0, &[0x7E, 0xF7]).is_none());
    }
}
