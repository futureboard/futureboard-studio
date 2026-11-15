//! What the MIDI editor's Sampler Rack asks the studio for (see
//! `components::piano_roll::RackRequest`): the rack is the pads of one
//! track's Drum Sampler, so its rows open that instrument, load samples onto
//! its pads and set the pads' own mute, solo, gain and pan. The plug-in
//! host and the engine live here, not in the editor.

use gpui::{Context, Window};

use crate::components::piano_roll::RackRequest;

use super::StudioLayout;

impl StudioLayout {
    pub(super) fn handle_rack_request(
        &mut self,
        request: RackRequest,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        match request {
            RackRequest::OpenEditor { track_id } => match window {
                Some(window) => self.open_track_instrument(&track_id, window, cx),
                None => eprintln!("[SamplerRack] open editor asked for without a window"),
            },
            RackRequest::LoadPadSample {
                insert_id,
                pad,
                path,
                ..
            } => self.load_rack_pad_sample(insert_id, pad, path, cx),
            RackRequest::SetPad {
                track_id,
                insert_id,
                pad,
                control,
                value,
            } => self.set_rack_pad(&track_id, &insert_id, pad, control, value, cx),
        }
    }

    /// One pad control, the way the Drum Sampler's own knob sets it: to the
    /// DSP through the engine's realtime path, then into the state mirror
    /// that saves with the project and replays into a restarted host.
    #[cfg(feature = "builtin-plugin-editor")]
    fn set_rack_pad(
        &mut self,
        track_id: &str,
        insert_id: &str,
        pad: usize,
        control: crate::components::piano_roll::PadControl,
        value: f32,
        cx: &mut Context<Self>,
    ) {
        use crate::components::piano_roll::PadControl;
        let field = match control {
            PadControl::Mute => "Mute",
            PadControl::Solo => "Solo",
            PadControl::Gain => "Gain",
            PadControl::Pan => "Pan",
            PadControl::Note => "Note",
            PadControl::Output => "Output",
        };
        let Some(index) = drumsampler::ui_param_index(&format!("pad{pad}{field}")) else {
            return;
        };
        let plugin_id = self
            .timeline
            .read(cx)
            .state
            .find_track(track_id)
            .and_then(|track| track.inserts.iter().find(|slot| slot.id == insert_id))
            .and_then(|slot| slot.plugin_id.clone());
        let Some(plugin_id) = plugin_id else {
            return;
        };
        if let Some(engine) = self.audio_bridge.engine.as_ref() {
            if let Err(error) = engine.set_insert_param(
                track_id.to_string(),
                insert_id.to_string(),
                index.to_string(),
                value,
            ) {
                eprintln!("[SamplerRack] pad {pad} {field} not sent: {error}");
            }
        }
        crate::components::builtin_plugin_editor::builtin_state_apply(
            &plugin_id, insert_id, index, value,
        );
        self.note_plugin_state_edited(cx);
        // A pad sent to another output gets that output's mixer strip; an
        // output nothing uses any more loses its strip.
        if control == PadControl::Output {
            self.sync_builtin_output_strips(cx);
        }
        cx.notify();
    }

    #[cfg(not(feature = "builtin-plugin-editor"))]
    fn set_rack_pad(
        &mut self,
        _track_id: &str,
        _insert_id: &str,
        _pad: usize,
        _control: crate::components::piano_roll::PadControl,
        _value: f32,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Loads `path` (or a file picked now) onto `pad`: copied into the Drum
    /// Sampler's Samples folder and sent to its instance in the plug-in
    /// host, the way a file dropped on the pad in the Drum Sampler is. The
    /// host's answer records the sample's name on the pad.
    fn load_rack_pad_sample(
        &mut self,
        insert_id: String,
        pad: usize,
        path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = path else {
            // Picked in the Browser, not a file dialog (see `browser_pick`).
            let studio = cx.weak_entity();
            crate::browser_pick::request(
                cx,
                format!("Sampler Rack · Pad {}", pad + 1),
                &crate::components::quick_sampler_window::SAMPLE_EXTENSIONS,
                move |path, cx| {
                    let _ = studio.update(cx, |this, cx| {
                        this.load_rack_pad_sample(insert_id, pad, Some(path), cx)
                    });
                },
            );
            return;
        };
        #[cfg(feature = "builtin-plugin-editor")]
        {
            use crate::components::builtin_plugin_files::{self as files, BuiltinFileKind};
            let Some(runtime) = self.plugin_editors.bridge_runtime.clone() else {
                eprintln!("[SamplerRack] the plug-in host is not running yet");
                return;
            };
            let samples_dir = files::plugin_files_root(drumsampler::PLUGIN_NAME)
                .join(BuiltinFileKind::Samples.dir_name());
            cx.spawn(async move |_this, cx| {
                let stored = cx
                    .background_executor()
                    .spawn(async move {
                        crate::components::builtin_plugin_editor_window::read_dropped_sample(
                            &samples_dir,
                            &path,
                        )
                    })
                    .await;
                let (name, bytes) = match stored {
                    Ok(stored) => stored,
                    Err(error) => {
                        eprintln!("[SamplerRack] pad {pad}: {error}");
                        return;
                    }
                };
                use base64::Engine as _;
                let command = SpherePluginHost::ipc::HostCommand::LoadBuiltinDrumSample {
                    plugin_instance_id: insert_id.clone(),
                    pad_index: pad as u32,
                    name,
                    audio_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                };
                match runtime.lock() {
                    Ok(mut bridge) => {
                        if let Err(error) = bridge.send_raw(&command) {
                            eprintln!(
                                "[SamplerRack] pad {pad} load not sent insert={insert_id}: {error}"
                            );
                        }
                    }
                    Err(_) => eprintln!("[SamplerRack] pad load dropped: bridge runtime poisoned"),
                }
            })
            .detach();
        }
        #[cfg(not(feature = "builtin-plugin-editor"))]
        let _ = (insert_id, pad, path, cx);
    }
}
