//! MIDI and plug-in preview notes sent outside the arrangement.

use super::*;

impl EngineInner {
    pub fn midi_preview_note_on(
        &self,
        track_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::MidiPreviewNoteOn {
            track_id,
            channel,
            pitch,
            velocity,
        })
    }

    pub fn midi_preview_note_off(
        &self,
        track_id: String,
        channel: u8,
        pitch: u8,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::MidiPreviewNoteOff {
            track_id,
            channel,
            pitch,
        })
    }

    pub fn midi_preview_control_change(
        &self,
        track_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::MidiPreviewControlChange {
            track_id,
            channel,
            controller,
            value,
        })
    }

    pub fn midi_preview_all_notes_off(&self, track_id: String) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::MidiPreviewAllNotesOff { track_id })
    }

    pub fn plugin_preview_note_on(
        &self,
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::PluginPreviewNoteOn {
            track_id,
            plugin_instance_id,
            channel,
            pitch,
            velocity,
        })
    }

    pub fn plugin_preview_note_off(
        &self,
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::PluginPreviewNoteOff {
            track_id,
            plugin_instance_id,
            channel,
            pitch,
        })
    }

    pub fn plugin_preview_control_change(
        &self,
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::PluginPreviewControlChange {
            track_id,
            plugin_instance_id,
            channel,
            controller,
            value,
        })
    }

    pub fn plugin_preview_all_notes_off(
        &self,
        track_id: String,
        plugin_instance_id: String,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::PluginPreviewAllNotesOff {
            track_id,
            plugin_instance_id,
        })
    }

    /// 14-bit pitch bend preview for the track's in-process instrument.
    pub fn midi_preview_pitch_bend(
        &self,
        track_id: String,
        channel: u8,
        value: u16,
    ) -> Result<(), SphereAudioError> {
        self.plugin_preview_pitch_bend(track_id, String::new(), channel, value)
    }

    pub fn plugin_preview_pitch_bend(
        &self,
        track_id: String,
        plugin_instance_id: String,
        channel: u8,
        value: u16,
    ) -> Result<(), SphereAudioError> {
        self.send_command(EngineCommand::PluginPreviewPitchBend {
            track_id,
            plugin_instance_id,
            channel,
            value,
        })
    }
}
