//! What the MIDI editor's Sampler Rack asks the studio for (see
//! `components::piano_roll::RackRequest`): new rows, instrument editors,
//! samples and live channel controls. The editor owns the pattern; these need
//! the plug-in host, the audio engine or new tracks.

use gpui::{Context, Window};

use crate::components::piano_roll::RackRequest;
use crate::components::timeline::timeline_state::{
    self, CreateTrackOptions, InputMonitorMode, InsertPluginFormat, TrackEditScope, TrackType,
};
use SpherePluginHost::PluginFormat as RegistryPluginFormat;

use super::StudioLayout;

/// The instrument a new rack row gets: the built-in Quick Sampler.
const RACK_ROW_INSTRUMENT_ID: &str = "builtin:quicksampler";

/// Name of the group made around a track that starts a rack.
const RACK_GROUP_NAME: &str = "Sampler Rack";

impl StudioLayout {
    pub(super) fn handle_rack_request(
        &mut self,
        request: RackRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match request {
            RackRequest::AddRow { track_id } => self.add_rack_row(&track_id, cx),
            RackRequest::OpenEditor { track_id } => {
                self.open_track_instrument(&track_id, window, cx);
            }
            RackRequest::LoadSample { track_id, path } => {
                self.load_rack_row_sample(&track_id, path, window, cx);
            }
            RackRequest::TrackParam {
                track_id,
                param,
                value,
            } => {
                // Mute, solo, volume and pan are live controls: the engine
                // takes them per block, never through a graph rebuild.
                self.mark_dirty_view_only();
                if let Some(engine) = self.audio_bridge.engine.as_ref() {
                    let _ = engine.update_track_param(&track_id, param, value);
                }
                self.push_mixer_snapshot_to_window(cx);
            }
        }
    }

    /// A new rack row after the last: an Instrument track with the Quick
    /// Sampler in `track_id`'s group, which is made around `track_id` first
    /// when it has none. One undo step.
    fn add_rack_row(&mut self, track_id: &str, cx: &mut Context<Self>) {
        let catalog =
            super::window_ops::add_track_instrument_plugins_from_catalog(&self.plugin_catalog);
        let Some(reg) = catalog
            .iter()
            .find(|p| {
                p.id == RACK_ROW_INSTRUMENT_ID
                    || p.class_id.as_deref() == Some(RACK_ROW_INSTRUMENT_ID)
            })
            .cloned()
        else {
            eprintln!("[SamplerRack] the Quick Sampler is not in the plug-in catalog");
            return;
        };
        let edit = self.begin_track_edit(TrackEditScope::track_list(), cx);
        let made = self.timeline.update(cx, |timeline, cx| {
            let state = &mut timeline.state;
            let group_id = match state.find_track(track_id)?.parent_group_id.clone() {
                Some(group) => group,
                None => {
                    state.select_track(track_id);
                    state.group_selected_tracks(RACK_GROUP_NAME.to_string())?
                }
            };
            let rows = state
                .tracks
                .iter()
                .filter(|t| t.parent_group_id.as_deref() == Some(group_id.as_str()))
                .count();
            let id = state.create_track(CreateTrackOptions {
                track_type: TrackType::Instrument,
                name: format!("Sampler {}", rows + 1),
                color: state.track_color_for_index(state.tracks.len()),
                volume: timeline_state::volume::db_to_norm(0.0),
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            state.assign_track_to_group(&id, &group_id);
            let slot_id = state.add_insert(&id)?;
            let format = match reg.format {
                RegistryPluginFormat::Vst3 => InsertPluginFormat::Vst3,
                RegistryPluginFormat::Vst2 => InsertPluginFormat::Vst2,
                RegistryPluginFormat::Clap => InsertPluginFormat::Clap,
                RegistryPluginFormat::Au => InsertPluginFormat::Au,
                RegistryPluginFormat::Lv2 => InsertPluginFormat::Lv2,
                RegistryPluginFormat::Unknown => InsertPluginFormat::Unknown,
            };
            state.set_insert_plugin(
                &id,
                &slot_id,
                reg.class_id.clone().unwrap_or_else(|| reg.id.clone()),
                Some(reg.path.clone()),
                format,
                Some(reg.vendor.clone()).filter(|vendor| !vendor.trim().is_empty()),
                reg.name.clone(),
            );
            state.set_insert_plugin_role(&id, &slot_id, true);
            cx.notify();
            Some((id, slot_id))
        });
        self.commit_track_edit("Add Sampler Row", edit, cx);
        let Some((id, slot_id)) = made else {
            return;
        };
        // A built-in instrument lives in the plug-in host: without a load
        // request its editor has no instance to send samples or notes to.
        if !self.load_bridge_insert_for_slot(&id, &slot_id, cx) {
            self.audio_bridge.project_dirty = true;
            self.schedule_audio_project_sync(cx, true, "sampler_rack_add_row");
        }
        self.mark_dirty();
        cx.notify();
    }

    /// Opens the row's Quick Sampler and loads `path` into it, or asks for a
    /// file when there is none: the editor's own load, which copies the file
    /// into the plug-in's Samples folder and sends it to the host.
    fn load_rack_row_sample(
        &mut self,
        track_id: &str,
        path: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_track_instrument(track_id, window, cx);
        #[cfg(feature = "builtin-plugin-editor")]
        {
            let insert_id = self
                .timeline
                .read(cx)
                .state
                .find_track(track_id)
                .and_then(|track| track.instrument_insert())
                .map(|slot| slot.id.clone());
            let Some(handle) = insert_id
                .and_then(|id| self.plugin_editors.quick_sampler.get(&id))
                .copied()
            else {
                return;
            };
            let _ = handle.update(cx, |editor, _window, cx| match path {
                Some(path) => editor.import_sample(path, cx),
                None => editor.browse_sample(cx),
            });
        }
        #[cfg(not(feature = "builtin-plugin-editor"))]
        let _ = path;
    }
}
