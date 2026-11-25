//! A track's MIDI FX chain (MIDI effects and generators between its MIDI and
//! its instrument), as the project and the UI hold it. The engine runs it
//! (`sphere_directaudioengine::RuntimeMidiFx`); this is what is saved, undone
//! and turned into the snapshot.

use sphere_midi_fx::DeviceKind;

use super::{InsertPluginFormat, InsertSlotState, TimelineState, TrackState, TrackType};

/// `MidiFxSlotState::device` of a hosted MIDI-effect plug-in.
pub const MIDI_FX_PLUGIN_DEVICE: &str = "plugin";

/// One device in a track's MIDI FX chain.
#[derive(Debug, Clone, PartialEq)]
pub struct MidiFxSlotState {
    /// Unique on its track, never reused there (the engine keeps a slot's
    /// running device across edits by this id).
    pub id: String,
    /// `DeviceKind::id`, or [`MIDI_FX_PLUGIN_DEVICE`].
    pub device: String,
    pub enabled: bool,
    /// Every parameter's value by its stable id, in the device's order.
    /// Empty for a plug-in, whose parameters are its own.
    pub params: Vec<(String, f32)>,
    /// A hosted plug-in: its instance, described like an insert (identity,
    /// opaque state, load status). Its `id` is the bridge instance id,
    /// unique across the project like an insert's, so the plug-in host, the
    /// editor and state capture treat it as one.
    pub plugin: Option<InsertSlotState>,
}

impl MidiFxSlotState {
    /// A slot for `kind` with default values.
    pub fn new(id: String, kind: DeviceKind) -> Self {
        Self {
            id,
            device: kind.id().to_string(),
            enabled: true,
            params: kind
                .params()
                .iter()
                .map(|p| (p.id.to_string(), p.default))
                .collect(),
            plugin: None,
        }
    }

    /// A slot hosting `plugin`.
    pub fn hosting(id: String, plugin: InsertSlotState) -> Self {
        Self {
            id,
            device: MIDI_FX_PLUGIN_DEVICE.to_string(),
            enabled: true,
            params: Vec::new(),
            plugin: Some(plugin),
        }
    }

    /// The device, when this build knows it.
    pub fn kind(&self) -> Option<DeviceKind> {
        DeviceKind::from_id(&self.device)
    }

    /// Value of parameter `id`, or its default when the slot has none saved.
    pub fn param(&self, id: &str) -> Option<f32> {
        let kind = self.kind()?;
        let desc = kind.params().iter().find(|p| p.id == id)?;
        Some(
            self.params
                .iter()
                .find(|(k, _)| k == id)
                .map(|(_, v)| desc.clamp(*v))
                .unwrap_or(desc.default),
        )
    }
}

/// Tracks whose MIDI reaches an instrument they host.
pub fn track_accepts_midi_fx(track: &TrackState) -> bool {
    matches!(track.track_type, TrackType::Instrument | TrackType::Midi)
}

fn next_slot_id(track: &TrackState) -> String {
    let n = track
        .midi_fx
        .iter()
        .filter_map(|s| s.id.strip_prefix("mfx-")?.parse::<u32>().ok())
        .max()
        .map_or(1, |n| n + 1);
    format!("mfx-{n}")
}

impl TimelineState {
    fn midi_fx_track_mut(&mut self, track_id: &str) -> Option<&mut TrackState> {
        self.tracks
            .iter_mut()
            .find(|t| t.id == track_id && track_accepts_midi_fx(t))
    }

    /// Add `kind` to `track_id`'s chain. A generator goes first (after any
    /// generators already there): it makes the notes the effects then shape.
    /// An effect goes last. Returns the new slot's id.
    /// Add a hosted MIDI-effect plug-in at the end of `track_id`'s chain.
    /// Returns `(slot id, plug-in instance id)`; the caller loads the
    /// instance.
    pub fn add_midi_fx_plugin(
        &mut self,
        track_id: &str,
        plugin_id: String,
        plugin_path: Option<std::path::PathBuf>,
        plugin_format: InsertPluginFormat,
        vendor: Option<String>,
        display_name: String,
    ) -> Option<(String, String)> {
        let instance_id = self.next_insert_slot_id_for(track_id);
        let track = self.midi_fx_track_mut(track_id)?;
        let id = next_slot_id(track);
        let mut plugin = InsertSlotState::empty(instance_id.clone());
        plugin.plugin_id = Some(plugin_id);
        plugin.plugin_path = plugin_path;
        plugin.plugin_format = Some(plugin_format);
        plugin.vendor = vendor;
        plugin.display_name = display_name;
        track
            .midi_fx
            .push(MidiFxSlotState::hosting(id.clone(), plugin));
        Some((id, instance_id))
    }

    /// The plug-in instance `instance_id` in `track_id`'s MIDI FX chain.
    pub fn midi_fx_plugin(&self, track_id: &str, instance_id: &str) -> Option<&InsertSlotState> {
        self.tracks
            .iter()
            .find(|t| t.id == track_id)?
            .midi_fx
            .iter()
            .filter_map(|s| s.plugin.as_ref())
            .find(|p| p.id == instance_id)
    }

    pub fn midi_fx_plugin_mut(
        &mut self,
        track_id: &str,
        instance_id: &str,
    ) -> Option<&mut InsertSlotState> {
        self.tracks
            .iter_mut()
            .find(|t| t.id == track_id)?
            .midi_fx
            .iter_mut()
            .filter_map(|s| s.plugin.as_mut())
            .find(|p| p.id == instance_id)
    }

    /// Every hosted MIDI FX plug-in as `(track id, instance id)`.
    pub fn midi_fx_plugin_instances(&self) -> Vec<(String, String)> {
        self.tracks
            .iter()
            .flat_map(|t| {
                t.midi_fx
                    .iter()
                    .filter_map(|s| s.plugin.as_ref())
                    .map(|p| (t.id.clone(), p.id.clone()))
            })
            .collect()
    }

    pub fn add_midi_fx(&mut self, track_id: &str, kind: DeviceKind) -> Option<String> {
        let track = self.midi_fx_track_mut(track_id)?;
        let id = next_slot_id(track);
        let at = if kind.is_generator() {
            track
                .midi_fx
                .iter()
                .take_while(|s| s.kind().is_some_and(DeviceKind::is_generator))
                .count()
        } else {
            track.midi_fx.len()
        };
        track
            .midi_fx
            .insert(at, MidiFxSlotState::new(id.clone(), kind));
        Some(id)
    }

    pub fn remove_midi_fx(&mut self, track_id: &str, slot_id: &str) -> bool {
        let Some(track) = self.midi_fx_track_mut(track_id) else {
            return false;
        };
        let before = track.midi_fx.len();
        track.midi_fx.retain(|s| s.id != slot_id);
        track.midi_fx.len() != before
    }

    /// Move `slot_id` to position `to` (clamped).
    pub fn move_midi_fx(&mut self, track_id: &str, slot_id: &str, to: usize) -> bool {
        let Some(track) = self.midi_fx_track_mut(track_id) else {
            return false;
        };
        let Some(from) = track.midi_fx.iter().position(|s| s.id == slot_id) else {
            return false;
        };
        let slot = track.midi_fx.remove(from);
        let to = to.min(track.midi_fx.len());
        track.midi_fx.insert(to, slot);
        from != to
    }

    pub fn set_midi_fx_enabled(&mut self, track_id: &str, slot_id: &str, enabled: bool) -> bool {
        let Some(slot) = self
            .midi_fx_track_mut(track_id)
            .and_then(|t| t.midi_fx.iter_mut().find(|s| s.id == slot_id))
        else {
            return false;
        };
        let changed = slot.enabled != enabled;
        slot.enabled = enabled;
        changed
    }

    /// Store a parameter value (clamped to the parameter's range). Returns
    /// the stored value.
    pub fn set_midi_fx_param(
        &mut self,
        track_id: &str,
        slot_id: &str,
        param_id: &str,
        value: f32,
    ) -> Option<f32> {
        let slot = self
            .midi_fx_track_mut(track_id)?
            .midi_fx
            .iter_mut()
            .find(|s| s.id == slot_id)?;
        let desc = slot.kind()?.params().iter().find(|p| p.id == param_id)?;
        let value = desc.clamp(value);
        match slot.params.iter_mut().find(|(k, _)| k == param_id) {
            Some(entry) => entry.1 = value,
            None => slot.params.push((param_id.to_string(), value)),
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_instrument_track() -> (TimelineState, String) {
        let mut state = TimelineState::default();
        let id = state.create_midi_track();
        (state, id)
    }

    fn devices(state: &TimelineState, track_id: &str) -> Vec<String> {
        state
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .unwrap()
            .midi_fx
            .iter()
            .map(|s| s.device.clone())
            .collect()
    }

    #[test]
    fn generators_go_first_and_effects_last() {
        let (mut s, t) = state_with_instrument_track();
        s.add_midi_fx(&t, DeviceKind::Chord).unwrap();
        s.add_midi_fx(&t, DeviceKind::PatternGenerator).unwrap();
        s.add_midi_fx(&t, DeviceKind::Scale).unwrap();
        s.add_midi_fx(&t, DeviceKind::RandomMelody).unwrap();
        assert_eq!(
            devices(&s, &t),
            ["pattern-generator", "random-melody", "chord", "scale"]
        );
    }

    #[test]
    fn slot_ids_are_never_reused_on_a_track() {
        let (mut s, t) = state_with_instrument_track();
        let a = s.add_midi_fx(&t, DeviceKind::Chord).unwrap();
        let b = s.add_midi_fx(&t, DeviceKind::Scale).unwrap();
        assert!(s.remove_midi_fx(&t, &b));
        let c = s.add_midi_fx(&t, DeviceKind::Scale).unwrap();
        assert_eq!((a.as_str(), c.as_str()), ("mfx-1", "mfx-2"));
        assert!(s.remove_midi_fx(&t, &a));
        assert_eq!(s.add_midi_fx(&t, DeviceKind::Chord).unwrap(), "mfx-3");
    }

    #[test]
    fn params_are_stored_clamped_and_read_back() {
        let (mut s, t) = state_with_instrument_track();
        let slot = s.add_midi_fx(&t, DeviceKind::Transpose).unwrap();
        assert_eq!(
            s.set_midi_fx_param(&t, &slot, "semitones", 99.0),
            Some(48.0)
        );
        assert_eq!(s.set_midi_fx_param(&t, &slot, "no-such", 1.0), None);
        let track = s.tracks.iter().find(|tr| tr.id == t).unwrap();
        assert_eq!(track.midi_fx[0].param("semitones"), Some(48.0));
        assert!(s.set_midi_fx_enabled(&t, &slot, false));
        assert!(!s.set_midi_fx_enabled(&t, &slot, false), "no change");
    }

    #[test]
    fn audio_tracks_take_no_midi_fx() {
        let mut s = TimelineState::default();
        let audio = s.create_audio_track();
        assert!(s.add_midi_fx(&audio, DeviceKind::Chord).is_none());
    }

    #[test]
    fn a_plugin_slot_gets_a_project_unique_instance_id() {
        let (mut s, t) = state_with_instrument_track();
        s.add_midi_fx(&t, DeviceKind::Chord).unwrap();
        let (slot, instance) = s
            .add_midi_fx_plugin(
                &t,
                "class".into(),
                None,
                InsertPluginFormat::Vst3,
                None,
                "Arp Plug".into(),
            )
            .unwrap();
        assert_eq!(slot, "mfx-2");
        assert_eq!(devices(&s, &t), ["chord", MIDI_FX_PLUGIN_DEVICE]);
        assert!(instance.starts_with("insert-"));
        assert_eq!(
            s.find_insert_slot(&t, &instance).unwrap().display_name,
            "Arp Plug"
        );
        assert_eq!(
            s.midi_fx_plugin_instances(),
            vec![(t.clone(), instance.clone())]
        );
        // The next insert anywhere never takes the plug-in's id.
        let next = s.add_insert(&t).unwrap();
        assert_ne!(next, instance);
    }

    #[test]
    fn a_duplicated_track_hosts_its_own_plugin_instance() {
        let (mut s, t) = state_with_instrument_track();
        let (_, instance) = s
            .add_midi_fx_plugin(
                &t,
                "class".into(),
                None,
                InsertPluginFormat::Vst3,
                None,
                "Arp Plug".into(),
            )
            .unwrap();
        let clone = s
            .clone_track(&t, true, &std::collections::HashMap::new())
            .unwrap();
        let copy = s
            .tracks
            .iter()
            .find(|tr| tr.id == clone.track_id)
            .unwrap()
            .midi_fx[0]
            .plugin
            .clone()
            .unwrap();
        assert_ne!(copy.id, instance, "two tracks never share an instance");
        assert_eq!(copy.display_name, "Arp Plug");
        // Listed with the inserts, so the caller loads it.
        assert!(clone.inserts.contains(&(instance, copy.id)));
    }

    #[test]
    fn move_reorders_within_bounds() {
        let (mut s, t) = state_with_instrument_track();
        let a = s.add_midi_fx(&t, DeviceKind::Chord).unwrap();
        s.add_midi_fx(&t, DeviceKind::Scale).unwrap();
        assert!(s.move_midi_fx(&t, &a, 99));
        assert_eq!(devices(&s, &t), ["scale", "chord"]);
    }
}
