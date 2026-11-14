//! ARA menu entries and command dispatch.
//!
//! ARA commands carry a plug-in id in the command string (`ara:bind:<id>`), so
//! they cannot be matched as literals in the main dispatcher and are handled
//! here first.

use gpui::Context;

use super::ara_studio::AraPluginChoice;
use super::StudioLayout;
use crate::components::context_menu::ContextMenuEntry;

/// Binds one clip to an ARA plug-in.
pub(crate) const ARA_BIND_PREFIX: &str = "ara:bind:";
/// Binds every unbound audio clip on a track to an ARA plug-in.
pub(crate) const ARA_BIND_TRACK_PREFIX: &str = "ara:bind-track:";
/// Opens the bound plug-in's editor for a clip.
pub(crate) const ARA_EDIT: &str = "ara:edit";
/// Removes a clip's ARA binding.
pub(crate) const ARA_REMOVE: &str = "ara:remove";
/// Removes every ARA binding on a track.
pub(crate) const ARA_REMOVE_TRACK: &str = "ara:remove-track";

fn entry(label: String, command: String, enabled: bool) -> ContextMenuEntry {
    if enabled {
        ContextMenuEntry::item(label, command)
    } else {
        ContextMenuEntry::disabled_item(label, command)
    }
}

impl StudioLayout {
    /// What the ARA section shows when it has no plug-in to offer.
    ///
    /// "The catalog has not loaded yet" and "nothing installed is ARA-capable"
    /// are different problems with different fixes, and telling a user to
    /// rescan a folder that is already full of ARA plug-ins is worse than
    /// saying nothing.
    fn ara_empty_entry(&self) -> ContextMenuEntry {
        if self.plugin_catalog.available.is_none() {
            ContextMenuEntry::disabled_item("Loading plug-ins…", "noop")
        } else {
            ContextMenuEntry::item("No ARA plug-ins found — Scan Plug-ins…", "plugins:scan")
        }
    }
}

fn is_audio(clip: &crate::components::timeline::timeline_state::ClipState) -> bool {
    matches!(
        clip.clip_type,
        crate::components::timeline::timeline_state::ClipType::Audio { .. }
    )
}

impl StudioLayout {
    /// ARA section of an audio clip's context menu.
    ///
    /// Present on every audio clip so the feature is discoverable; when no ARA
    /// plug-in is known it offers the scan instead of vanishing. Only an
    /// unsupported platform removes the section entirely.
    pub(crate) fn ara_clip_menu_entries(
        &self,
        clip_id: &str,
        clip_exists: bool,
        cx: &Context<Self>,
    ) -> Vec<ContextMenuEntry> {
        if !super::ara_ops::AraState::is_supported() {
            // ARA cannot run on this platform at all; an empty section would
            // read as a missing feature rather than an unsupported one.
            return Vec::new();
        }
        let mut choices = self.ara_plugin_choices();
        // A MIDI clip is material only for an instrument (a voice synth): it
        // is where that plug-in's region sits. It offers only those, and says
        // nothing at all where there are none or the track's plug-in is one
        // that processes recordings — a MIDI clip there is not part of it.
        let state = &self.timeline.read(cx).state;
        let midi = state
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter().map(move |clip| (track, clip)))
            .find(|(_, clip)| clip.id == clip_id)
            .filter(|(_, clip)| !is_audio(clip))
            .map(|(track, _)| track.ara.as_ref().map(|binding| binding.generator));
        if let Some(bound_generator) = midi {
            choices.retain(|choice| choice.is_instrument);
            match bound_generator {
                Some(false) => return Vec::new(),
                None if choices.is_empty() => return Vec::new(),
                _ => {}
            }
        }
        let mut entries = vec![
            ContextMenuEntry::Separator,
            ContextMenuEntry::Header("ARA".to_string()),
        ];
        if choices.is_empty() && self.ara_binding_for_clip(clip_id, cx).is_none() {
            entries.push(self.ara_empty_entry());
            return entries;
        }
        // ARA belongs to the track, so these act on the track this clip sits on
        // rather than pretending a clip can be bound on its own.
        match self.ara_binding_for_clip(clip_id, cx) {
            Some((_, plugin_name)) => {
                entries.push(entry(
                    format!("Edit with {plugin_name}"),
                    ARA_EDIT.to_string(),
                    clip_exists,
                ));
                entries.push(ContextMenuEntry::danger_item(
                    format!("Remove {plugin_name} from Track"),
                    ARA_REMOVE,
                ));
            }
            None => {
                for choice in choices {
                    entries.push(entry(
                        format!("{} (whole track)", choice.name),
                        format!("{ARA_BIND_PREFIX}{}", choice.id),
                        clip_exists,
                    ));
                }
            }
        }
        entries
    }

    /// ARA section of a track header's context menu.
    ///
    /// Binds or unbinds every audio clip on the track at once — the track-level
    /// counterpart of the per-clip commands.
    pub(crate) fn ara_track_menu_entries(
        &self,
        track_id: &str,
        track_exists: bool,
        cx: &Context<Self>,
    ) -> Vec<ContextMenuEntry> {
        if !super::ara_ops::AraState::is_supported() {
            return Vec::new();
        }
        let choices: Vec<AraPluginChoice> = self.ara_plugin_choices();
        let (audio_clips, bound_plugin) = {
            let state = &self.timeline.read(cx).state;
            match state.tracks.iter().find(|track| track.id == track_id) {
                Some(track) => (
                    track.clips.iter().filter(|clip| is_audio(clip)).count(),
                    track.ara.as_ref().map(|binding| binding.plugin_id.clone()),
                ),
                None => (0, None),
            }
        };

        let mut entries = vec![
            ContextMenuEntry::Separator,
            ContextMenuEntry::Header("ARA".to_string()),
        ];
        if choices.is_empty() {
            entries.push(self.ara_empty_entry());
            return entries;
        }
        for choice in choices {
            // One track, one ARA plug-in. The active one is disabled rather than
            // hidden, so the track's current processor stays visible.
            let selected = bound_plugin.as_deref() == Some(choice.id.as_str());
            let label = if selected {
                format!("{} (active)", choice.name)
            } else {
                choice.name.clone()
            };
            // A plug-in that processes recordings needs one on the track; an
            // instrument (a voice synth) makes its own sound and works on the
            // track's MIDI clips, so it can be bound to any track.
            let has_material = audio_clips > 0 || choice.is_instrument;
            entries.push(entry(
                label,
                format!("{ARA_BIND_TRACK_PREFIX}{}", choice.id),
                track_exists && has_material && !selected,
            ));
        }
        if bound_plugin.is_some() {
            entries.push(ContextMenuEntry::danger_item(
                "Remove ARA from Track",
                ARA_REMOVE_TRACK,
            ));
        }
        entries
    }

    /// Handles an ARA command.
    ///
    /// Returns `false` when `command_id` is not an ARA command, so the caller
    /// falls through to its own match.
    pub(crate) fn dispatch_ara_command(
        &mut self,
        command_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(plugin_id) = command_id.strip_prefix(ARA_BIND_PREFIX) {
            let plugin_id = plugin_id.to_string();
            if let Some(track_id) = self
                .context_clip_id_or_selected(cx)
                .and_then(|clip_id| self.track_of_selected_clip(&clip_id, cx))
            {
                self.bind_track_to_ara(&track_id, &plugin_id, cx);
            }
            return true;
        }
        if let Some(plugin_id) = command_id.strip_prefix(ARA_BIND_TRACK_PREFIX) {
            let plugin_id = plugin_id.to_string();
            if let Some(track_id) = self.context_track_id_or_selected(cx) {
                self.bind_track_to_ara(&track_id, &plugin_id, cx);
            }
            return true;
        }
        match command_id {
            ARA_EDIT => {
                if let Some(clip_id) = self.context_clip_id_or_selected(cx) {
                    self.open_ara_editor(&clip_id, cx);
                }
                true
            }
            ARA_REMOVE => {
                if let Some(track_id) = self
                    .context_clip_id_or_selected(cx)
                    .and_then(|clip_id| self.track_of_selected_clip(&clip_id, cx))
                {
                    self.unbind_track_from_ara(&track_id, cx);
                }
                true
            }
            ARA_REMOVE_TRACK => {
                if let Some(track_id) = self.context_track_id_or_selected(cx) {
                    self.unbind_track_from_ara(&track_id, cx);
                }
                true
            }
            _ => false,
        }
    }

    /// Track that owns a clip, so the clip menu can act on it.
    fn track_of_selected_clip(&self, clip_id: &str, cx: &gpui::App) -> Option<String> {
        let state = &self.timeline.read(cx).state;
        state
            .tracks
            .iter()
            .find(|track| track.clips.iter().any(|clip| clip.id == clip_id))
            .map(|track| track.id.clone())
    }
}
