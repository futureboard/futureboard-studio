use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::components::progress_dialog::ProgressBarValue;
use SpherePluginHost::ipc::{HostCommand, HostEvent};
use SpherePluginHost::plugin_host_client::{
    plugin_host_bridge_enabled, ClientEvent, PluginHostClient, PluginHostClientError,
};
use SpherePluginHost::plugin_host_lifecycle::{self, BridgeHostManager};

#[derive(Debug, Clone)]
pub(crate) struct BridgePluginDescriptor {
    pub track_id: String,
    pub insert_id: String,
    pub plugin_path: String,
    pub class_id: String,
    pub display_name: String,
    /// Module format label (`"VST3"` / `"VST2"`) forwarded to the host so it
    /// instantiates through the right native bridge. `None` leaves the host to
    /// detect it from `plugin_path`.
    pub format: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct BridgeLoadedPlugin {
    pub descriptor: BridgePluginDescriptor,
    pub host_pid: Option<u32>,
    confirmed: bool,
}

pub(crate) struct PluginBridgeRuntime {
    client: PluginHostClient,
    host_pid: Option<u32>,
    loaded: HashMap<String, BridgeLoadedPlugin>,
    queued_events: VecDeque<ClientEvent>,
    /// Stage 1: the engine-owned (sample_rate, block) most recently pushed to the
    /// host via `ConfigureAudioBridge`. `None` until the first configure so the
    /// next `LoadPlugin` sends it first.
    audio_bridge_config: Option<(u32, u32)>,
    /// Stage 2: one shared-memory audio region per insert instance. Each region
    /// carries its own `request_seq` / `done_seq` so serial FX chains on one
    /// track do not clobber each other's handshake.
    shared_audio: HashMap<String, Arc<SpherePluginHost::audio_bridge::SharedAudioRegion>>,
    /// Producer wake event shared with the host process: the audio-callback
    /// sink signals it after every `request_seq` bump so the host renders on
    /// demand instead of polling on a timer tick. One event per engine/host
    /// pid pair (all insert regions share it — the producer sweeps every
    /// region per wake). `None` when creation failed; the host then falls
    /// back to its poll loop.
    kick: Option<Arc<SpherePluginHost::audio_bridge::BridgeKickEvent>>,
}

pub(crate) type SharedPluginBridgeRuntime = Arc<Mutex<PluginBridgeRuntime>>;

pub(crate) fn bridge_enabled() -> bool {
    plugin_host_bridge_enabled()
}

pub(super) fn legacy_in_process_enabled() -> bool {
    SpherePluginHost::plugin_host_client::legacy_in_process_enabled()
}

/// Named shared-memory region for one insert instance.
pub(crate) fn bridge_region_name(instance_id: &str) -> String {
    format!(
        "Local\\FutureboardAudioBridge-{}__{}",
        std::process::id(),
        instance_id
    )
}

#[cfg(test)]
mod tests {
    use super::{bridge_region_name, normalize_persisted_au_state};

    #[test]
    fn bridge_region_names_are_unique_per_insert_instance() {
        let a = bridge_region_name("insert-track1-1");
        let b = bridge_region_name("insert-track1-2");
        assert_ne!(
            a, b,
            "each insert instance must get its own shared region name"
        );
        assert!(a.contains("insert-track1-1"));
        assert!(b.contains("insert-track1-2"));
    }

    #[test]
    fn raw_audio_unit_state_is_preserved() {
        let raw = b"bplist00opaque-audio-unit-state";
        assert_eq!(normalize_persisted_au_state(raw), raw);
    }

    #[test]
    fn legacy_vst3_wrapped_audio_unit_state_is_unwrapped() {
        let raw = b"bplist00legacy-audio-unit-state";
        let packed = DirectAudio::Vst3PluginState {
            component: raw.to_vec(),
            controller: Vec::new(),
        }
        .to_packed_bytes();
        assert_eq!(normalize_persisted_au_state(&packed), raw);
    }
}

/// Early AU bridge builds accidentally persisted ClassInfo inside the VST3
/// `FBV3` envelope. Accept both forms so existing projects load, while all new
/// saves keep the Audio Unit's opaque bytes unchanged.
fn normalize_persisted_au_state(state: &[u8]) -> Vec<u8> {
    DirectAudio::Vst3PluginState::from_packed_bytes(state)
        .filter(|legacy| legacy.controller.is_empty() && !legacy.component.is_empty())
        .map(|legacy| legacy.component)
        .unwrap_or_else(|| state.to_vec())
}

impl PluginBridgeRuntime {
    pub fn ensure_shared(
        slot: &mut Option<SharedPluginBridgeRuntime>,
    ) -> Result<SharedPluginBridgeRuntime, PluginHostClientError> {
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone());
        }
        eprintln!("[plugin-bridge] ensure_host running=false -> spawn");
        let mut client = PluginHostClient::spawn_bridge()?;
        let host_pid = Some(client.pid());
        // The host emits Ready on startup; retain it for any caller that wants
        // to poll, but do not block insert on a second handshake.
        let _ = client.ping();
        // Producer wake event for this engine/host pair (the host derives the
        // same name from `--parent-pid` + its own pid). CreateEventW opens the
        // existing event if the host won the race, so order does not matter.
        let kick_name = SpherePluginHost::audio_bridge::bridge_kick_event_name(
            std::process::id(),
            client.pid(),
        );
        let kick = match SpherePluginHost::audio_bridge::BridgeKickEvent::create_named(&kick_name) {
            Ok(event) => {
                eprintln!("[plugin-bridge] kick event ready name={kick_name}");
                Some(Arc::new(event))
            }
            Err(error) => {
                eprintln!(
                    "[plugin-bridge] kick event create failed name={kick_name} error={error}; host will poll"
                );
                None
            }
        };
        let runtime = Arc::new(Mutex::new(Self {
            client,
            host_pid,
            loaded: HashMap::new(),
            queued_events: VecDeque::new(),
            audio_bridge_config: None,
            shared_audio: HashMap::new(),
            kick,
        }));
        *slot = Some(runtime.clone());
        Ok(runtime)
    }

    pub fn host_pid(&self) -> Option<u32> {
        self.host_pid
    }

    /// Whether this instance has a realtime sink, without building one.
    ///
    /// [`Self::audio_sink_for`] allocates an `Arc` and takes a reference on the
    /// shared audio region; asking it `.is_some()` on every previewed note —
    /// twice per piano-roll click — paid for a sink that was dropped on the
    /// next line.
    pub fn has_audio_sink(&self, instance_id: &str) -> bool {
        self.shared_audio.contains_key(instance_id)
    }

    /// Stage 3b: realtime sink for one insert instance.
    pub fn audio_sink_for(
        &self,
        instance_id: &str,
    ) -> Option<DirectAudio::plugin_bridge::SharedPluginBridgeSink> {
        let region = self.shared_audio.get(instance_id)?;
        Some(
            SpherePluginHost::plugin_bridge_sink::SharedRegionSink::into_shared(
                region.clone(),
                self.kick.clone(),
            ),
        )
    }

    /// What one bridged plug-in costs and delays: `(cpu_share, latency_samples)`.
    ///
    /// Read straight from the shared region the host writes into, not from the
    /// engine's graph. The engine's control-side `RuntimeProject` is a clone
    /// taken when the stream opened -- the sink installed for a plug-in loaded
    /// afterwards, and the block times the callback records, both land on the
    /// copy the audio thread owns, so asking that side reports zero forever.
    /// This is the same memory the host publishes into and the only live view of
    /// it the UI has.
    ///
    /// `cpu_share` is `None` until a block has actually been processed: an
    /// unmeasured plug-in has no cost to report, and reporting 0% would say it
    /// is free.
    pub fn instance_load(&self, instance_id: &str) -> Option<(Option<f32>, u32)> {
        use std::sync::atomic::Ordering;
        let bridge = self.shared_audio.get(instance_id)?.bridge();
        let latency = bridge.latency_samples.load(Ordering::Relaxed);
        let micros = bridge.last_process_micros.load(Ordering::Relaxed);
        let sample_rate = bridge.sample_rate.load(Ordering::Relaxed);
        // The block's own wall-clock budget: the same duration means very
        // different things at 64 frames and at 1024. `block_frames` is what the
        // last block actually ran at; before any block it is 0, so fall back to
        // what the region was prepared for.
        let frames = match bridge.block_frames.load(Ordering::Relaxed) {
            0 => bridge.max_block_size.load(Ordering::Relaxed),
            frames => frames,
        };
        if micros == 0 || frames == 0 || sample_rate == 0 {
            return Some((None, latency));
        }
        let deadline_micros = frames as f64 * 1_000_000.0 / sample_rate as f64;
        Some((Some((micros as f64 / deadline_micros) as f32), latency))
    }

    pub fn loaded_descriptor(&self, instance: &str) -> Option<BridgeLoadedPlugin> {
        self.loaded
            .get(instance)
            .filter(|loaded| loaded.confirmed)
            .cloned()
    }

    pub fn has_load_request(&self, instance: &str) -> bool {
        self.loaded.contains_key(instance)
    }

    pub fn loaded_instance_ids(&self) -> Vec<String> {
        self.loaded
            .iter()
            .filter(|(_, loaded)| loaded.confirmed)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn loaded_for_track(&self, track_id: &str) -> Option<BridgeLoadedPlugin> {
        self.loaded
            .values()
            .find(|loaded| loaded.confirmed && loaded.descriptor.track_id == track_id)
            .cloned()
    }

    pub fn mark_plugin_loaded(&mut self, instance: &str) -> bool {
        let Some(loaded) = self.loaded.get_mut(instance) else {
            eprintln!("[plugin-bridge] confirmed load for unknown instance={instance}");
            return false;
        };
        loaded.confirmed = true;
        true
    }

    pub fn mark_plugin_output_channels(&mut self, instance: &str, output_channels: u32) {
        if let Some(region) = self.shared_audio.get(instance) {
            let channels = output_channels.max(1);
            region.bridge().set_plugin_output_channels(channels);
            eprintln!(
                "[plugin-bridge] plugin output metadata instance={instance} channels={channels}"
            );
        }
    }

    pub fn mark_plugin_load_failed(&mut self, instance: &str) {
        self.loaded.remove(instance);
        self.shared_audio.remove(instance);
    }

    /// Stage 1: push the engine-owned sample rate / block size to the host so it
    /// follows them for plugin DSP. Idempotent — only re-sent when the config
    /// actually changes. The host replies `AudioBridgeConfigured`.
    pub fn configure_audio_bridge(
        &mut self,
        sample_rate: u32,
        max_block_size: u32,
    ) -> Result<(), PluginHostClientError> {
        if self.audio_bridge_config == Some((sample_rate, max_block_size)) {
            return Ok(());
        }
        eprintln!(
            "[plugin-bridge] sending ConfigureAudioBridge sample_rate={sample_rate} max_block_size={max_block_size} (engine owns)"
        );
        self.client
            .configure_audio_bridge(sample_rate, max_block_size)?;
        self.audio_bridge_config = Some((sample_rate, max_block_size));
        Ok(())
    }

    /// Stage 2: create a named shared-memory region for one insert and map it in
    /// the host. Idempotent per `instance_id`.
    fn establish_shared_audio_for_instance(
        &mut self,
        instance_id: &str,
        sample_rate: u32,
        max_block_size: u32,
    ) {
        if self.shared_audio.contains_key(instance_id) {
            return;
        }
        use SpherePluginHost::audio_bridge::SharedAudioRegion;
        let name = bridge_region_name(instance_id);
        match SharedAudioRegion::create_named(&name, sample_rate, max_block_size, 2) {
            Ok(region) => {
                let bytes = region.bytes();
                eprintln!(
                    "[plugin-bridge] shared audio region created instance={instance_id} name={name} bytes={bytes} sr={sample_rate} block={max_block_size}"
                );
                eprintln!(
                    "[plugin-bridge] sending AttachSharedAudio instance={instance_id} name={name} bytes={bytes}"
                );
                match self
                    .client
                    .attach_shared_audio(name.clone(), bytes, instance_id.to_string())
                {
                    Ok(()) => {
                        self.shared_audio
                            .insert(instance_id.to_string(), Arc::new(region));
                    }
                    Err(error) => {
                        eprintln!(
                            "[plugin-bridge] AttachSharedAudio send failed instance={instance_id}: {error}"
                        )
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "[plugin-bridge] shared audio region create failed instance={instance_id}: {error}"
                )
            }
        }
    }

    /// `state_json` is the persisted built-in state blob (e.g. a
    /// `RodhareistState` JSON) the host applies to the DSP before publishing
    /// it to the audio producer — the race-free restore path for project open
    /// and host respawn. `None` = start at defaults.
    pub fn send_load_builtin_plugin(
        &mut self,
        descriptor: BridgePluginDescriptor,
        sample_rate: u32,
        max_block_size: u32,
        state_json: Option<String>,
    ) -> Result<(), PluginHostClientError> {
        let _ = self.configure_audio_bridge(sample_rate, max_block_size);
        if self.loaded.contains_key(&descriptor.insert_id) {
            eprintln!(
                "[plugin-bridge] LoadBuiltinPlugin skipped instance={} reason=already_loaded",
                descriptor.insert_id
            );
            return Ok(());
        }
        let instance = descriptor.insert_id.clone();
        eprintln!(
            "[plugin-bridge] sending LoadBuiltinPlugin instance={} plugin={} state_bytes={}",
            instance,
            descriptor.class_id,
            state_json.as_deref().map(str::len).unwrap_or(0)
        );
        self.establish_shared_audio_for_instance(&instance, sample_rate, max_block_size);
        self.client.load_builtin_plugin(
            instance.clone(),
            descriptor.class_id.clone(),
            sample_rate,
            max_block_size,
            state_json,
        )?;
        self.loaded.insert(
            instance,
            BridgeLoadedPlugin {
                descriptor,
                host_pid: self.host_pid,
                confirmed: false,
            },
        );
        Ok(())
    }

    /// `descriptor.class_id` is the AU component id; Audio Units have no module
    /// path. `state` is the persisted opaque ClassInfo blob, sent base64-encoded
    /// with the load so the host applies it before the instance reaches the
    /// audio producer — the same race-free restore the built-ins use.
    pub fn send_load_au_plugin(
        &mut self,
        descriptor: BridgePluginDescriptor,
        sample_rate: u32,
        max_block_size: u32,
        state: Option<&[u8]>,
    ) -> Result<(), PluginHostClientError> {
        use base64::Engine as _;
        let _ = self.configure_audio_bridge(sample_rate, max_block_size);
        if self.loaded.contains_key(&descriptor.insert_id) {
            eprintln!(
                "[plugin-bridge] LoadAudioUnit skipped instance={} reason=already_loaded",
                descriptor.insert_id
            );
            return Ok(());
        }
        let instance = descriptor.insert_id.clone();
        let state = state
            .filter(|bytes| !bytes.is_empty())
            .map(normalize_persisted_au_state);
        let state_b64 = state
            .as_deref()
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));
        eprintln!(
            "[plugin-bridge] sending LoadAudioUnit instance={} component={} state_bytes={}",
            instance,
            descriptor.class_id,
            state.as_deref().map(<[u8]>::len).unwrap_or(0)
        );
        self.establish_shared_audio_for_instance(&instance, sample_rate, max_block_size);
        self.client.load_au_plugin(
            instance.clone(),
            descriptor.class_id.clone(),
            sample_rate,
            max_block_size,
            state_b64,
        )?;
        self.loaded.insert(
            instance,
            BridgeLoadedPlugin {
                descriptor,
                host_pid: self.host_pid,
                confirmed: false,
            },
        );
        Ok(())
    }

    pub fn send_load_plugin(
        &mut self,
        descriptor: BridgePluginDescriptor,
        sample_rate: u32,
        max_block_size: u32,
    ) -> Result<(), PluginHostClientError> {
        let _ = self.configure_audio_bridge(sample_rate, max_block_size);
        if self.loaded.contains_key(&descriptor.insert_id) {
            eprintln!(
                "[plugin-bridge] LoadPlugin skipped instance={} reason=already_loaded",
                descriptor.insert_id
            );
            return Ok(());
        }
        let instance = descriptor.insert_id.clone();
        eprintln!(
            "[plugin-bridge] sending LoadPlugin instance={} path={}",
            instance, descriptor.plugin_path
        );
        self.establish_shared_audio_for_instance(&instance, sample_rate, max_block_size);
        let plugin_path = descriptor.plugin_path.clone();
        let class_id = descriptor.class_id.clone();
        let format = descriptor.format.clone();
        self.client.load_plugin(
            instance.clone(),
            plugin_path,
            class_id,
            sample_rate,
            max_block_size,
            format,
        )?;
        self.loaded.insert(
            instance.clone(),
            BridgeLoadedPlugin {
                descriptor: descriptor.clone(),
                host_pid: self.host_pid,
                confirmed: false,
            },
        );
        let input_channels = 2u32;
        let output_channels = 2u32;
        eprintln!(
            "[plugin-bridge] sending PrepareProcessing instance={instance} sr={sample_rate} block={max_block_size}"
        );
        let prepare = self.client.prepare_processing(
            instance,
            sample_rate,
            max_block_size,
            input_channels,
            output_channels,
        );
        if prepare.is_err() {
            self.loaded.remove(&descriptor.insert_id);
            self.shared_audio.remove(&descriptor.insert_id);
        }
        prepare
    }

    pub fn open_editor_with_parent(
        &mut self,
        plugin_instance_id: String,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        let loaded = self.loaded.get(&plugin_instance_id).cloned();
        let (path, class_id) = loaded
            .as_ref()
            .map(|plugin| {
                (
                    plugin.descriptor.plugin_path.clone(),
                    plugin.descriptor.class_id.clone(),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new()));
        if let Some(plugin) = loaded {
            let display_title = format!(
                "{} - {}",
                plugin.descriptor.display_name, plugin.descriptor.track_id
            );
            eprintln!(
                "[OpenEditor/IPC] track_id={} slot_id={} instance_id={} owner_hwnd=0x{parent_hwnd:x} plugin={}",
                plugin.descriptor.track_id,
                plugin.descriptor.insert_id,
                plugin_instance_id,
                plugin.descriptor.display_name
            );
            return self.client.open_editor_with_metadata(
                plugin.descriptor.track_id,
                None,
                None,
                plugin.descriptor.insert_id.clone(),
                plugin_instance_id,
                path,
                class_id.clone(),
                Some(class_id),
                display_title,
                parent_hwnd,
                parent_hwnd,
                width,
                height,
                dpi,
            );
        }
        let (path, class_id) = self
            .loaded
            .get(&plugin_instance_id)
            .map(|plugin| {
                (
                    plugin.descriptor.plugin_path.clone(),
                    plugin.descriptor.class_id.clone(),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new()));
        eprintln!("[plugin-bridge] OpenEditorWithParentHwnd hwnd=0x{parent_hwnd:x}");
        self.client.open_editor(
            plugin_instance_id,
            path,
            class_id,
            parent_hwnd,
            width,
            height,
            dpi,
        )
    }

    pub fn prepare_editor_view(
        &mut self,
        plugin_instance_id: String,
    ) -> Result<(), PluginHostClientError> {
        let (path, class_id) = self
            .loaded
            .get(&plugin_instance_id)
            .map(|plugin| {
                (
                    plugin.descriptor.plugin_path.clone(),
                    plugin.descriptor.class_id.clone(),
                )
            })
            .unwrap_or_else(|| (String::new(), String::new()));
        eprintln!("[plugin-bridge] PrepareEditorView instance={plugin_instance_id}");
        self.client
            .prepare_editor_view(plugin_instance_id, path, class_id)
    }

    pub fn confirm_editor_content_ready(
        &mut self,
        plugin_instance_id: String,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        eprintln!(
            "[plugin-bridge] ConfirmEditorContentReady instance={plugin_instance_id} hwnd=0x{parent_hwnd:x} size={width}x{height}"
        );
        self.client.confirm_editor_content_ready(
            plugin_instance_id,
            parent_hwnd,
            width,
            height,
            dpi,
        )
    }

    pub fn preview_note_on(
        &mut self,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), PluginHostClientError> {
        eprintln!(
            "[plugin-bridge] sending PreviewNoteOn instance={plugin_instance_id} ch={channel} pitch={pitch} vel={velocity}"
        );
        self.client
            .preview_note_on(plugin_instance_id, channel, pitch, velocity)
    }

    pub fn preview_note_off(
        &mut self,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
    ) -> Result<(), PluginHostClientError> {
        eprintln!(
            "[plugin-bridge] sending PreviewNoteOff instance={plugin_instance_id} ch={channel} pitch={pitch}"
        );
        self.client
            .preview_note_off(plugin_instance_id, channel, pitch)
    }

    pub fn preview_control_change(
        &mut self,
        plugin_instance_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), PluginHostClientError> {
        self.client
            .preview_control_change(plugin_instance_id, channel, controller, value)
    }

    pub fn preview_all_notes_off(
        &mut self,
        plugin_instance_id: String,
    ) -> Result<(), PluginHostClientError> {
        eprintln!("[plugin-bridge] sending PreviewAllNotesOff instance={plugin_instance_id}");
        self.client.preview_all_notes_off(plugin_instance_id)
    }

    pub fn midi_panic(&mut self, plugin_instance_id: String) -> Result<(), PluginHostClientError> {
        eprintln!("[plugin-bridge] sending MidiPanic instance={plugin_instance_id}");
        self.client.midi_panic(plugin_instance_id)
    }

    pub fn resize_editor(&mut self, plugin_instance_id: String, width: u32, height: u32, dpi: u32) {
        let _ = self
            .client
            .resize_editor(plugin_instance_id, width, height, dpi);
    }

    /// Push a prepared [`HostCommand::SetEditorChrome`] to the host.
    ///
    /// Prepared by the caller rather than assembled here: everything in it —
    /// the presets, the readouts, the theme — belongs to the editor window that
    /// already knows them, and this is only the pipe.
    pub fn set_editor_chrome(&mut self, chrome: SpherePluginHost::ipc::HostCommand) {
        let _ = self.client.set_editor_chrome(chrome);
    }

    pub fn close_editor(&mut self, plugin_instance_id: String) {
        eprintln!("[plugin-bridge] CloseEditor instance={plugin_instance_id}");
        let _ = self.client.close_editor(plugin_instance_id);
    }

    pub fn unload_plugin(&mut self, plugin_instance_id: String) {
        eprintln!("[plugin-bridge] UnloadPlugin instance={plugin_instance_id}");
        let _ = self.client.unload_plugin(plugin_instance_id.clone());
        self.loaded.remove(&plugin_instance_id);
        self.shared_audio.remove(&plugin_instance_id);
    }

    /// True while this instance id is still tracked as a loaded bridge plugin.
    /// Used by the removal invariant check to prove the instance is gone.
    pub fn is_loaded(&self, plugin_instance_id: &str) -> bool {
        self.loaded.contains_key(plugin_instance_id)
    }

    /// Capture current plugin states from the host for `instance_ids`
    /// (request/response over IPC with a bounded wait — call on save, not per
    /// frame). Unrelated events arriving while waiting are queued for the
    /// normal `drain_events` pump. VST3 state is returned in the host's packed
    /// component/controller form; Audio Unit ClassInfo remains opaque raw
    /// bytes. Instances with no state or that timed out are simply absent.
    pub fn request_plugin_states(
        &mut self,
        instance_ids: &[String],
        timeout: std::time::Duration,
    ) -> HashMap<String, Vec<u8>> {
        use base64::Engine as _;
        let mut results = HashMap::new();
        let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
        for instance_id in instance_ids {
            if !self.loaded.contains_key(instance_id) {
                continue;
            }
            match self.client.get_plugin_state(instance_id.clone()) {
                Ok(()) => {
                    pending.insert(instance_id.clone());
                }
                Err(error) => eprintln!(
                    "[plugin-bridge] GetPluginState send failed instance={instance_id}: {error}"
                ),
            }
        }
        let deadline = std::time::Instant::now() + timeout;
        while !pending.is_empty() && std::time::Instant::now() < deadline {
            let Some(event) = self.client.try_recv_event() else {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            };
            match event {
                ClientEvent::Host(HostEvent::PluginState {
                    plugin_instance_id,
                    ok,
                    component_b64,
                    controller_b64,
                }) => {
                    pending.remove(&plugin_instance_id);
                    if !ok {
                        eprintln!(
                            "[plugin-bridge] GetPluginState failed instance={plugin_instance_id}"
                        );
                        continue;
                    }
                    let decode = |b64: &str| {
                        base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .unwrap_or_default()
                    };
                    let component = decode(&component_b64);
                    let controller = decode(&controller_b64);
                    let is_audio_unit = self
                        .loaded
                        .get(&plugin_instance_id)
                        .is_some_and(|loaded| loaded.descriptor.class_id.starts_with("au:"));
                    eprintln!(
                        "[plugin-bridge] plugin state captured instance={plugin_instance_id} component_bytes={} controller_bytes={}",
                        component.len(),
                        controller.len()
                    );
                    if is_audio_unit {
                        if !component.is_empty() {
                            results.insert(plugin_instance_id, component);
                        }
                    } else {
                        let state = DirectAudio::Vst3PluginState {
                            component,
                            controller,
                        };
                        if !state.is_empty() {
                            results.insert(plugin_instance_id, state.to_packed_bytes());
                        }
                    }
                }
                ClientEvent::Disconnected => {
                    self.queued_events.push_back(ClientEvent::Disconnected);
                    break;
                }
                other => self.queued_events.push_back(other),
            }
        }
        if !pending.is_empty() {
            eprintln!(
                "[plugin-bridge] GetPluginState timed out pending={} timeout_ms={}",
                pending.len(),
                timeout.as_millis()
            );
        }
        results
    }

    /// Restore state (from the project file) onto a loaded instance. VST3 uses
    /// its packed component/controller envelope; AU accepts raw ClassInfo and
    /// legacy FBV3-wrapped ClassInfo. The host applies it serialized against
    /// block production and replies `PluginStateSet`.
    pub fn send_plugin_state(
        &mut self,
        instance_id: &str,
        packed: &[u8],
    ) -> Result<(), PluginHostClientError> {
        use base64::Engine as _;
        let is_audio_unit = self
            .loaded
            .get(instance_id)
            .is_some_and(|loaded| loaded.descriptor.class_id.starts_with("au:"));
        if is_audio_unit {
            let state = normalize_persisted_au_state(packed);
            eprintln!(
                "[plugin-bridge] sending SetPluginState instance={instance_id} au_state_bytes={}",
                state.len()
            );
            return self.client.set_plugin_state(
                instance_id,
                base64::engine::general_purpose::STANDARD.encode(state),
                String::new(),
            );
        }
        let Some(state) = DirectAudio::Vst3PluginState::from_packed_bytes(packed) else {
            eprintln!(
                "[plugin-bridge] SetPluginState skipped instance={instance_id}: unrecognized packed state ({} bytes)",
                packed.len()
            );
            return Ok(());
        };
        eprintln!(
            "[plugin-bridge] sending SetPluginState instance={instance_id} component_bytes={} controller_bytes={}",
            state.component.len(),
            state.controller.len()
        );
        self.client.set_plugin_state(
            instance_id,
            base64::engine::general_purpose::STANDARD.encode(&state.component),
            base64::engine::general_purpose::STANDARD.encode(&state.controller),
        )
    }

    /// Ask the host to enumerate VST3 parameters for a loaded instance.
    pub fn request_plugin_parameters(
        &mut self,
        plugin_instance_id: &str,
    ) -> Result<(), PluginHostClientError> {
        if !self.loaded.contains_key(plugin_instance_id) {
            return Ok(());
        }
        self.client.get_plugin_parameters(plugin_instance_id)
    }

    pub fn poll(&mut self) {
        while let Some(event) = self.client.try_recv_event() {
            match &event {
                ClientEvent::Host(HostEvent::Ready { pid, .. })
                | ClientEvent::Host(HostEvent::Pong { pid }) => {
                    self.host_pid = Some(*pid);
                }
                _ => {}
            }
            self.queued_events.push_back(event);
        }
    }

    pub fn drain_events(&mut self) -> Vec<ClientEvent> {
        self.poll();
        self.queued_events.drain(..).collect()
    }

    pub fn send_raw(&mut self, command: &HostCommand) -> Result<(), PluginHostClientError> {
        self.client.send(command)
    }

    /// Latest built-in DSP telemetry for `instance_id`'s shared region, plus
    /// footer status straight from the region header. `None` when no region is
    /// mapped for the instance, or when its DSP publishes no telemetry at all
    /// (an EQ has nothing to meter). Pure atomic loads — cheap enough for a
    /// ~30 Hz UI poll.
    pub fn builtin_meter_frame(
        &self,
        instance_id: &str,
    ) -> Option<SpherePluginHost::audio_bridge::BuiltinMeterFrame> {
        self.shared_audio
            .get(instance_id)?
            .bridge()
            .builtin_meters()
    }

    /// Latest analyser frame for `instance_id`, as `(sequence, dB bins)`.
    /// `None` when no region is mapped or the host has published nothing yet.
    /// The caller compares the sequence against the last one it forwarded so an
    /// unchanged frame is never re-sent to the page.
    pub fn builtin_spectrum_frame(
        &self,
        instance_id: &str,
    ) -> Option<(u32, [f32; SpherePluginHost::spectrum::SPECTRUM_BINS])> {
        self.shared_audio.get(instance_id)?.bridge().spectrum()
    }

    /// Region-header status for the footer: (sample_rate, block_frames,
    /// latency_samples, tempo_bpm). `None` when no region is mapped.
    ///
    /// The tempo comes from the same transport block the DSP is processing
    /// against, so an editor that shows a musical time (EchoSpace's note
    /// divisions) cannot print a length the delay line is not running at.
    pub fn builtin_host_status(&self, instance_id: &str) -> Option<(u32, u32, u32, f64)> {
        use std::sync::atomic::Ordering;
        let region = self.shared_audio.get(instance_id)?;
        let bridge = region.bridge();
        Some((
            bridge.sample_rate.load(Ordering::Relaxed),
            bridge.max_block_size.load(Ordering::Relaxed),
            bridge.latency_samples.load(Ordering::Relaxed),
            bridge.load_transport().tempo_bpm,
        ))
    }

    /// Graceful shutdown of the shared bridge host. Drains the runtime slot so
    /// the [`PluginHostClient`] is dropped and process handles are released.
    pub fn shutdown_shared(slot: &mut Option<SharedPluginBridgeRuntime>) {
        let _ = shutdown_bridge_runtime(
            slot.take(),
            plugin_host_lifecycle::HOST_SHUTDOWN_TIMEOUT,
            |_, _| {},
        );
    }
}

#[derive(Debug, Clone, Default)]
pub struct BridgeShutdownReport {
    pub hosts_shutdown: usize,
    pub hosts_killed: usize,
    pub warnings: Vec<String>,
}

/// Shut down a bridge runtime, waiting for the host process to exit.
pub(crate) fn shutdown_bridge_runtime(
    runtime: Option<SharedPluginBridgeRuntime>,
    timeout: std::time::Duration,
    mut progress: impl FnMut(String, ProgressBarValue),
) -> BridgeShutdownReport {
    let mut report = BridgeShutdownReport::default();
    let host_count = BridgeHostManager::global().host_count();
    eprintln!("[plugin-bridge] shutdown begin hosts={host_count}");

    let Some(runtime) = runtime else {
        eprintln!("[plugin-bridge] shutdown complete");
        return report;
    };

    let host_pid = runtime.lock().ok().and_then(|bridge| bridge.host_pid());
    if let Some(pid) = host_pid {
        progress(
            format!("Waiting for plugin host pid={pid}"),
            ProgressBarValue::value(0.7),
        );
    }

    if let Ok(mut bridge) = runtime.lock() {
        let instance_ids = bridge.loaded_instance_ids();
        if let Some(pid) = bridge.host_pid() {
            BridgeHostManager::global().set_host_instances(pid, instance_ids);
        }
        plugin_host_lifecycle::shutdown_host_client_with_timeout(&mut bridge.client, timeout);
        bridge.client.join_reader();
        bridge.loaded.clear();
        bridge.shared_audio.clear();
        bridge.queued_events.clear();
        bridge.host_pid = None;
        if host_pid.is_some() {
            report.hosts_shutdown += 1;
        }
    }
    drop(runtime);

    BridgeHostManager::global().clear_hosts();
    eprintln!("[plugin-bridge] shutdown complete");
    report
}

/// Shut down every plugin-host child owned by the studio layout.
pub(crate) fn shutdown_plugin_bridge(slot: &mut Option<SharedPluginBridgeRuntime>) {
    PluginBridgeRuntime::shutdown_shared(slot);
}
