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

/// One plug-in host process and what it holds. The studio talks to hosts
/// only through the pool ([`PluginBridgeRuntime`]).
pub(crate) struct BridgeHost {
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
    /// The realtime sink handed out for each region, reused while the region
    /// is. A sink carries the freshness guard (`last_read_seq`); a new one per
    /// engine sync started from zero and could hand the engine a block the
    /// previous sink had already read. It also lets the engine see an
    /// unchanged sink as unchanged and skip re-installing it.
    audio_sinks: std::sync::Mutex<
        HashMap<
            String,
            (
                Arc<SpherePluginHost::audio_bridge::SharedAudioRegion>,
                DirectAudio::plugin_bridge::SharedPluginBridgeSink,
            ),
        >,
    >,
    /// Producer wake event shared with the host process: the audio-callback
    /// sink signals it after every `request_seq` bump so the host renders on
    /// demand instead of polling on a timer tick. One event per engine/host
    /// pid pair (all insert regions share it — the producer sweeps every
    /// region per wake). `None` when creation failed; the host then falls
    /// back to its poll loop.
    kick: Option<Arc<SpherePluginHost::audio_bridge::BridgeKickEvent>>,
    /// The process went away (its pipe closed). The pool takes it out.
    dead: bool,
    /// Shared with every realtime sink of this host's regions, cleared when
    /// it dies so they stop waiting on a producer that is gone.
    host_alive: Arc<std::sync::atomic::AtomicBool>,
}

pub(crate) type SharedPluginBridgeRuntime = Arc<Mutex<PluginBridgeRuntime>>;

/// What one [`PluginBridgeRuntime::request_plugin_states`] got back.
#[derive(Debug, Default)]
pub(crate) struct PluginStateCapture {
    /// Captured state per instance: the packed VST3 component/controller form,
    /// or an Audio Unit's raw ClassInfo bytes.
    pub states: HashMap<String, Vec<u8>>,
    /// Instances asked for their state that had not answered when the wait ran
    /// out (or when the host went away), sorted. Their slots keep whatever
    /// state was captured before, which may be older than the plug-in's.
    pub unanswered: Vec<String>,
}

pub(crate) fn bridge_enabled() -> bool {
    plugin_host_bridge_enabled()
}

pub(super) fn legacy_in_process_enabled() -> bool {
    SpherePluginHost::plugin_host_client::legacy_in_process_enabled()
}

/// Named shared-memory region for one insert instance. Unique per creation:
/// an instance reloaded into a fresh host after a crash needs a new region
/// while the engine may still hold the old one, and a name that is already
/// open cannot be created again.
pub(crate) fn bridge_region_name(instance_id: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "Local\\FutureboardAudioBridge-{}__{}__g{}",
        std::process::id(),
        instance_id,
        generation
    )
}

#[cfg(test)]
mod tests {
    use super::{
        bridge_region_name, normalize_persisted_au_state, BridgePluginDescriptor, HostIsolation,
    };

    fn descriptor(insert: &str, path: &str) -> BridgePluginDescriptor {
        BridgePluginDescriptor {
            track_id: "track-1".to_string(),
            insert_id: insert.to_string(),
            plugin_path: path.to_string(),
            class_id: "class".to_string(),
            display_name: "Plug".to_string(),
            format: None,
        }
    }

    /// Per module: two instances of one plug-in share a host, another plug-in
    /// gets its own, and the built-ins share theirs — so a crash reaches only
    /// the plug-in that crashed.
    #[test]
    fn instances_are_grouped_into_hosts_by_isolation() {
        let kontakt_a = descriptor("insert-1", "C:/VST3/Kontakt 7.vst3");
        let kontakt_b = descriptor("insert-2", "C:/vst3/kontakt 7.VST3");
        let serum = descriptor("insert-3", "C:/VST3/Serum.vst3");
        let module = HostIsolation::Module;
        assert_eq!(
            module.key_for(&kontakt_a, false),
            module.key_for(&kontakt_b, false)
        );
        assert_ne!(
            module.key_for(&kontakt_a, false),
            module.key_for(&serum, false)
        );
        assert_eq!(module.key_for(&serum, true), "builtin");
        assert_eq!(
            HostIsolation::Shared.key_for(&serum, false),
            HostIsolation::Shared.key_for(&kontakt_a, true)
        );
        assert_ne!(
            HostIsolation::Instance.key_for(&kontakt_a, false),
            HostIsolation::Instance.key_for(&kontakt_b, false)
        );
    }

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
        // A reload after a host crash gets a fresh name, since the old region
        // may still be open.
        assert_ne!(a, bridge_region_name("insert-track1-1"));
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

impl BridgeHost {
    fn spawn(key: String) -> Result<Self, PluginHostClientError> {
        eprintln!("[plugin-bridge] ensure_host key={key} -> spawn");
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
        Ok(Self {
            client,
            host_pid,
            loaded: HashMap::new(),
            queued_events: VecDeque::new(),
            audio_bridge_config: None,
            shared_audio: HashMap::new(),
            audio_sinks: std::sync::Mutex::new(HashMap::new()),
            kick,
            dead: false,
            host_alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    /// Marks the process gone: sinks stop waiting on it.
    fn abandon(&mut self) {
        self.dead = true;
        self.host_alive
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    fn shutdown(&mut self, timeout: std::time::Duration) {
        if let Some(pid) = self.host_pid {
            BridgeHostManager::global().set_host_instances(pid, self.loaded_instance_ids());
        }
        self.host_alive
            .store(false, std::sync::atomic::Ordering::Relaxed);
        plugin_host_lifecycle::shutdown_host_client_with_timeout(&mut self.client, timeout);
        self.client.join_reader();
        self.loaded.clear();
        self.shared_audio.clear();
        self.forget_audio_sinks();
        self.queued_events.clear();
        self.host_pid = None;
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
        let mut sinks = self.audio_sinks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_region, sink)) = sinks.get(instance_id) {
            if Arc::ptr_eq(cached_region, region) {
                return Some(sink.clone());
            }
        }
        let sink =
            SpherePluginHost::plugin_bridge_sink::SharedRegionSink::into_shared_with_liveness(
                region.clone(),
                self.kick.clone(),
                self.host_alive.clone(),
            );
        sinks.insert(instance_id.to_string(), (region.clone(), sink.clone()));
        Some(sink)
    }

    /// Drop an instance's region and the sink cached for it.
    fn remove_shared_audio(&mut self, instance_id: &str) {
        self.shared_audio.remove(instance_id);
        self.audio_sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(instance_id);
    }

    fn forget_audio_sinks(&mut self) {
        self.audio_sinks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
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

    /// Re-file every instance (loaded or still loading) under the track that
    /// now owns it, as `owner_of` reports. An insert moved to another channel
    /// keeps its instance id, and the host process keeps the instance, but the
    /// descriptor's track is what the MIDI fallback routes by
    /// (`loaded_for_track`). Returns how many changed.
    pub fn retarget_tracks<'a>(&mut self, owner_of: impl Fn(&str) -> Option<&'a str>) -> usize {
        let mut changed = 0;
        for (instance, loaded) in self.loaded.iter_mut() {
            let Some(owner) = owner_of(instance) else {
                continue;
            };
            if loaded.descriptor.track_id != owner {
                loaded.descriptor.track_id = owner.to_string();
                changed += 1;
            }
        }
        changed
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
        self.remove_shared_audio(instance);
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
            self.remove_shared_audio(&descriptor.insert_id);
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
        // Per note, on the live-input path: logged only on request.
        if preview_note_debug() {
            eprintln!(
                "[plugin-bridge] sending PreviewNoteOn instance={plugin_instance_id} ch={channel} pitch={pitch} vel={velocity}"
            );
        }
        self.client
            .preview_note_on(plugin_instance_id, channel, pitch, velocity)
    }

    pub fn preview_note_off(
        &mut self,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
    ) -> Result<(), PluginHostClientError> {
        if preview_note_debug() {
            eprintln!(
                "[plugin-bridge] sending PreviewNoteOff instance={plugin_instance_id} ch={channel} pitch={pitch}"
            );
        }
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
        self.remove_shared_audio(&plugin_instance_id);
    }

    /// True while this instance id is still tracked as a loaded bridge plugin.
    /// Used by the removal invariant check to prove the instance is gone.
    pub fn is_loaded(&self, plugin_instance_id: &str) -> bool {
        self.loaded.contains_key(plugin_instance_id)
    }

    /// Asks for the state of each of `instance_ids` this host holds; returns
    /// the ones asked. Pair with [`Self::collect_state_replies`].
    fn send_state_requests(
        &mut self,
        instance_ids: &[String],
    ) -> std::collections::HashSet<String> {
        let mut pending = std::collections::HashSet::new();
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
        pending
    }

    /// Collects the answers to [`Self::send_state_requests`] until `deadline`
    /// (request/response over IPC — call on save, not per frame). Unrelated
    /// events arriving meanwhile are queued for the normal `drain_events`
    /// pump. VST3 state comes back in the host's packed component/controller
    /// form; Audio Unit ClassInfo stays opaque raw bytes.
    fn collect_state_replies(
        &mut self,
        mut pending: std::collections::HashSet<String>,
        deadline: std::time::Instant,
        timeout: std::time::Duration,
    ) -> PluginStateCapture {
        let mut results = HashMap::new();
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
                    if let Some(packed) =
                        self.packed_state(&plugin_instance_id, &component_b64, &controller_b64)
                    {
                        results.insert(plugin_instance_id, packed);
                    }
                }
                ClientEvent::Disconnected => {
                    self.abandon();
                    break;
                }
                other => self.queued_events.push_back(other),
            }
        }
        let mut unanswered: Vec<String> = pending.into_iter().collect();
        unanswered.sort();
        if !unanswered.is_empty() {
            eprintln!(
                "[plugin-bridge] GetPluginState timed out pending={} timeout_ms={} instances={}",
                unanswered.len(),
                timeout.as_millis(),
                unanswered.join(",")
            );
        }
        PluginStateCapture {
            states: results,
            unanswered,
        }
    }

    /// A `PluginState` reply in the form the insert stores: VST3's packed
    /// component/controller envelope, or an Audio Unit's raw ClassInfo.
    /// `None` for an empty state.
    fn packed_state(
        &self,
        instance_id: &str,
        component_b64: &str,
        controller_b64: &str,
    ) -> Option<Vec<u8>> {
        use base64::Engine as _;
        let decode = |b64: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap_or_default()
        };
        let component = decode(component_b64);
        let controller = decode(controller_b64);
        let is_audio_unit = self
            .loaded
            .get(instance_id)
            .is_some_and(|loaded| loaded.descriptor.class_id.starts_with("au:"));
        eprintln!(
            "[plugin-bridge] plugin state captured instance={instance_id} component_bytes={} controller_bytes={}",
            component.len(),
            controller.len()
        );
        if is_audio_unit {
            return (!component.is_empty()).then_some(component);
        }
        let state = DirectAudio::Vst3PluginState {
            component,
            controller,
        };
        (!state.is_empty()).then(|| state.to_packed_bytes())
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
                // The pipe closed: the process is gone. Reported by the pool
                // as a lost host, not passed on as an event.
                ClientEvent::Disconnected => {
                    self.abandon();
                    continue;
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

    /// Latest stereo-image frame for `instance_id`, as `(sequence, frame)`.
    /// `None` when no region is mapped or its DSP measures no image.
    pub fn builtin_stereo_image_frame(
        &self,
        instance_id: &str,
    ) -> Option<(u32, SpherePluginHost::audio_bridge::StereoImageFrame)> {
        self.shared_audio.get(instance_id)?.bridge().stereo_image()
    }

    /// Latest per-pad levels for `instance_id`, as `(sequence, levels)`.
    /// `None` when no region is mapped or its DSP has no pads.
    pub fn builtin_pad_levels(
        &self,
        instance_id: &str,
    ) -> Option<(
        u32,
        [f32; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
    )> {
        self.shared_audio.get(instance_id)?.bridge().pad_levels()
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
}

// ── Host pool ────────────────────────────────────────────────────────────────
//
// Bridged plug-ins used to share one host process, so one plug-in crashing
// took every other one — the built-ins included — down with it, and nothing
// brought them back. The pool keeps several host processes, each a
// [`BridgeHost`], and routes every call by instance id to the host that loaded
// the instance. Which instances share a host is [`HostIsolation`]'s choice.
// A host that dies is taken out and reported through
// [`PluginBridgeRuntime::take_lost_hosts`]; the studio reloads what lived in
// it, into a fresh host, and nothing else is touched.

/// How bridged instances are spread over host processes
/// (`FUTUREBOARD_PLUGIN_HOST_ISOLATION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostIsolation {
    /// Every instance in one process: the old behaviour.
    Shared,
    /// One process per plug-in module; the built-ins share one of their own.
    /// Instances of one plug-in share its fate, and nothing else does.
    Module,
    /// One process per instance.
    Instance,
}

impl HostIsolation {
    fn current() -> Self {
        static MODE: std::sync::OnceLock<HostIsolation> = std::sync::OnceLock::new();
        *MODE.get_or_init(|| {
            match std::env::var("FUTUREBOARD_PLUGIN_HOST_ISOLATION")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "shared" => Self::Shared,
                "instance" => Self::Instance,
                _ => Self::Module,
            }
        })
    }

    fn key_for(self, descriptor: &BridgePluginDescriptor, builtin: bool) -> String {
        match self {
            Self::Shared => "shared".to_string(),
            Self::Instance => format!("instance:{}", descriptor.insert_id),
            Self::Module if builtin => "builtin".to_string(),
            Self::Module => {
                let module = if descriptor.plugin_path.is_empty() {
                    &descriptor.class_id
                } else {
                    &descriptor.plugin_path
                };
                format!("module:{}", module.to_ascii_lowercase())
            }
        }
    }
}

/// A host process that went away, with everything that was loaded in it.
#[derive(Debug, Clone)]
pub(crate) struct LostBridgeHost {
    pub key: String,
    pub pid: Option<u32>,
    /// Every instance the host held (loaded or still loading), with the
    /// descriptor it was loaded from.
    pub instances: Vec<BridgeLoadedPlugin>,
}

pub(crate) struct PluginBridgeRuntime {
    hosts: HashMap<String, BridgeHost>,
    /// Which host each instance lives in.
    instance_host: HashMap<String, String>,
    /// The engine-owned (sample_rate, block) last configured, for hosts
    /// spawned later.
    audio_bridge_config: Option<(u32, u32)>,
    /// Hosts that died since the studio last asked.
    lost: Vec<LostBridgeHost>,
}

impl PluginBridgeRuntime {
    /// The pool, created on first use. No process starts until an instance
    /// is loaded.
    pub fn ensure_shared(
        slot: &mut Option<SharedPluginBridgeRuntime>,
    ) -> Result<SharedPluginBridgeRuntime, PluginHostClientError> {
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone());
        }
        eprintln!(
            "[plugin-bridge] host pool ready isolation={:?}",
            HostIsolation::current()
        );
        let runtime = Arc::new(Mutex::new(Self {
            hosts: HashMap::new(),
            instance_host: HashMap::new(),
            audio_bridge_config: None,
            lost: Vec::new(),
        }));
        *slot = Some(runtime.clone());
        Ok(runtime)
    }

    fn host_for(&self, instance_id: &str) -> Option<&BridgeHost> {
        self.hosts.get(self.instance_host.get(instance_id)?)
    }

    fn host_for_mut(&mut self, instance_id: &str) -> Option<&mut BridgeHost> {
        let key = self.instance_host.get(instance_id)?;
        self.hosts.get_mut(key)
    }

    /// The host an instance about to load goes to, spawned when it is the
    /// first of its key.
    fn host_for_load(
        &mut self,
        descriptor: &BridgePluginDescriptor,
        builtin: bool,
    ) -> Result<&mut BridgeHost, PluginHostClientError> {
        let key = self
            .instance_host
            .get(&descriptor.insert_id)
            .filter(|key| self.hosts.contains_key(*key))
            .cloned()
            .unwrap_or_else(|| HostIsolation::current().key_for(descriptor, builtin));
        if !self.hosts.contains_key(&key) {
            let mut host = BridgeHost::spawn(key.clone())?;
            if let Some((sample_rate, block)) = self.audio_bridge_config {
                let _ = host.configure_audio_bridge(sample_rate, block);
            }
            self.hosts.insert(key.clone(), host);
        }
        self.instance_host
            .insert(descriptor.insert_id.clone(), key.clone());
        Ok(self.hosts.get_mut(&key).expect("host just ensured"))
    }

    fn not_loaded(instance_id: &str) -> PluginHostClientError {
        PluginHostClientError::Spawn(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            format!("no plugin host holds instance {instance_id}"),
        ))
    }

    /// A live host's pid: the one serving `instance_id` when given, else any.
    pub fn host_pid(&self) -> Option<u32> {
        self.hosts.values().find_map(|host| host.host_pid)
    }

    pub fn host_pid_for(&self, instance_id: &str) -> Option<u32> {
        self.host_for(instance_id).and_then(|host| host.host_pid)
    }

    pub fn has_audio_sink(&self, instance_id: &str) -> bool {
        self.host_for(instance_id)
            .is_some_and(|host| host.has_audio_sink(instance_id))
    }

    pub fn audio_sink_for(
        &self,
        instance_id: &str,
    ) -> Option<DirectAudio::plugin_bridge::SharedPluginBridgeSink> {
        self.host_for(instance_id)?.audio_sink_for(instance_id)
    }

    pub fn instance_load(&self, instance_id: &str) -> Option<(Option<f32>, u32)> {
        self.host_for(instance_id)?.instance_load(instance_id)
    }

    pub fn loaded_descriptor(&self, instance: &str) -> Option<BridgeLoadedPlugin> {
        self.host_for(instance)?.loaded_descriptor(instance)
    }

    pub fn has_load_request(&self, instance: &str) -> bool {
        self.host_for(instance)
            .is_some_and(|host| host.has_load_request(instance))
    }

    pub fn loaded_instance_ids(&self) -> Vec<String> {
        self.hosts
            .values()
            .flat_map(|host| host.loaded_instance_ids())
            .collect()
    }

    pub fn loaded_for_track(&self, track_id: &str) -> Option<BridgeLoadedPlugin> {
        self.hosts
            .values()
            .find_map(|host| host.loaded_for_track(track_id))
    }

    pub fn retarget_tracks<'a>(&mut self, owner_of: impl Fn(&str) -> Option<&'a str>) -> usize {
        self.hosts
            .values_mut()
            .map(|host| host.retarget_tracks(&owner_of))
            .sum()
    }

    pub fn mark_plugin_loaded(&mut self, instance: &str) -> bool {
        match self.host_for_mut(instance) {
            Some(host) => host.mark_plugin_loaded(instance),
            None => {
                eprintln!("[plugin-bridge] confirmed load for unknown instance={instance}");
                false
            }
        }
    }

    pub fn mark_plugin_output_channels(&mut self, instance: &str, output_channels: u32) {
        if let Some(host) = self.host_for_mut(instance) {
            host.mark_plugin_output_channels(instance, output_channels);
        }
    }

    pub fn mark_plugin_load_failed(&mut self, instance: &str) {
        if let Some(host) = self.host_for_mut(instance) {
            host.mark_plugin_load_failed(instance);
        }
        self.instance_host.remove(instance);
    }

    pub fn configure_audio_bridge(
        &mut self,
        sample_rate: u32,
        max_block_size: u32,
    ) -> Result<(), PluginHostClientError> {
        self.audio_bridge_config = Some((sample_rate, max_block_size));
        let mut result = Ok(());
        for host in self.hosts.values_mut() {
            if let Err(error) = host.configure_audio_bridge(sample_rate, max_block_size) {
                result = Err(error);
            }
        }
        result
    }

    pub fn send_load_builtin_plugin(
        &mut self,
        descriptor: BridgePluginDescriptor,
        sample_rate: u32,
        max_block_size: u32,
        state_json: Option<String>,
    ) -> Result<(), PluginHostClientError> {
        self.audio_bridge_config = Some((sample_rate, max_block_size));
        self.host_for_load(&descriptor, true)?
            .send_load_builtin_plugin(descriptor, sample_rate, max_block_size, state_json)
    }

    pub fn send_load_au_plugin(
        &mut self,
        descriptor: BridgePluginDescriptor,
        sample_rate: u32,
        max_block_size: u32,
        state: Option<&[u8]>,
    ) -> Result<(), PluginHostClientError> {
        self.audio_bridge_config = Some((sample_rate, max_block_size));
        self.host_for_load(&descriptor, false)?.send_load_au_plugin(
            descriptor,
            sample_rate,
            max_block_size,
            state,
        )
    }

    pub fn send_load_plugin(
        &mut self,
        descriptor: BridgePluginDescriptor,
        sample_rate: u32,
        max_block_size: u32,
    ) -> Result<(), PluginHostClientError> {
        self.audio_bridge_config = Some((sample_rate, max_block_size));
        self.host_for_load(&descriptor, false)?.send_load_plugin(
            descriptor,
            sample_rate,
            max_block_size,
        )
    }

    pub fn open_editor_with_parent(
        &mut self,
        plugin_instance_id: String,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.open_editor_with_parent(plugin_instance_id, parent_hwnd, width, height, dpi)
    }

    pub fn prepare_editor_view(
        &mut self,
        plugin_instance_id: String,
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.prepare_editor_view(plugin_instance_id)
    }

    pub fn confirm_editor_content_ready(
        &mut self,
        plugin_instance_id: String,
        parent_hwnd: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.confirm_editor_content_ready(plugin_instance_id, parent_hwnd, width, height, dpi)
    }

    pub fn preview_note_on(
        &mut self,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.preview_note_on(plugin_instance_id, channel, pitch, velocity)
    }

    pub fn preview_note_off(
        &mut self,
        plugin_instance_id: String,
        channel: u8,
        pitch: u8,
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.preview_note_off(plugin_instance_id, channel, pitch)
    }

    pub fn preview_control_change(
        &mut self,
        plugin_instance_id: String,
        channel: u8,
        controller: u8,
        value: u8,
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.preview_control_change(plugin_instance_id, channel, controller, value)
    }

    pub fn preview_all_notes_off(
        &mut self,
        plugin_instance_id: String,
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.preview_all_notes_off(plugin_instance_id)
    }

    pub fn midi_panic(&mut self, plugin_instance_id: String) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(&plugin_instance_id) else {
            return Err(Self::not_loaded(&plugin_instance_id));
        };
        host.midi_panic(plugin_instance_id)
    }

    pub fn resize_editor(&mut self, plugin_instance_id: String, width: u32, height: u32, dpi: u32) {
        if let Some(host) = self.host_for_mut(&plugin_instance_id) {
            host.resize_editor(plugin_instance_id, width, height, dpi);
        }
    }

    pub fn set_editor_chrome(&mut self, chrome: SpherePluginHost::ipc::HostCommand) {
        let target = match &chrome {
            HostCommand::SetEditorChrome {
                plugin_instance_id, ..
            } => self.instance_host.get(plugin_instance_id).cloned(),
            _ => None,
        };
        if let Some(host) = target.and_then(|key| self.hosts.get_mut(&key)) {
            host.set_editor_chrome(chrome);
        }
    }

    pub fn close_editor(&mut self, plugin_instance_id: String) {
        if let Some(host) = self.host_for_mut(&plugin_instance_id) {
            host.close_editor(plugin_instance_id);
        }
    }

    pub fn unload_plugin(&mut self, plugin_instance_id: String) {
        let Some(key) = self.instance_host.remove(&plugin_instance_id) else {
            return;
        };
        let Some(host) = self.hosts.get_mut(&key) else {
            return;
        };
        host.unload_plugin(plugin_instance_id);
        // A per-instance or per-module host with nothing left in it has no
        // reason to keep running.
        if key != "shared" && !self.instance_host.values().any(|other| *other == key) {
            if let Some(mut host) = self.hosts.remove(&key) {
                eprintln!("[plugin-bridge] host idle, shutting down key={key}");
                host.shutdown(plugin_host_lifecycle::HOST_SHUTDOWN_TIMEOUT);
            }
        }
    }

    pub fn is_loaded(&self, plugin_instance_id: &str) -> bool {
        self.host_for(plugin_instance_id)
            .is_some_and(|host| host.is_loaded(plugin_instance_id))
    }

    /// Asks every host that holds one of `instance_ids` first, then collects
    /// the answers under one shared deadline, so a save does not wait for the
    /// hosts one after another.
    pub fn request_plugin_states(
        &mut self,
        instance_ids: &[String],
        timeout: std::time::Duration,
    ) -> PluginStateCapture {
        let mut by_host: HashMap<String, Vec<String>> = HashMap::new();
        for instance_id in instance_ids {
            if let Some(key) = self.instance_host.get(instance_id) {
                by_host
                    .entry(key.clone())
                    .or_default()
                    .push(instance_id.clone());
            }
        }
        let mut asked: Vec<(String, std::collections::HashSet<String>)> = Vec::new();
        for (key, ids) in by_host {
            if let Some(host) = self.hosts.get_mut(&key) {
                asked.push((key, host.send_state_requests(&ids)));
            }
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut capture = PluginStateCapture::default();
        for (key, pending) in asked {
            if let Some(host) = self.hosts.get_mut(&key) {
                let part = host.collect_state_replies(pending, deadline, timeout);
                capture.states.extend(part.states);
                capture.unanswered.extend(part.unanswered);
            }
        }
        capture.unanswered.sort();
        capture
    }

    /// Asks for one instance's state without waiting for it; the answer comes
    /// through [`Self::drain_events`] as `PluginState`. For keeping a recent
    /// state between saves — never on the save path, which has to wait.
    pub fn request_plugin_state_async(&mut self, instance_id: &str) {
        let Some(host) = self.host_for_mut(instance_id) else {
            return;
        };
        if !host.is_loaded(instance_id) {
            return;
        }
        if let Err(error) = host.client.get_plugin_state(instance_id.to_string()) {
            eprintln!("[plugin-bridge] GetPluginState send failed instance={instance_id}: {error}");
        }
    }

    /// A drained `PluginState` event in the form the insert stores.
    pub fn packed_state_reply(
        &self,
        instance_id: &str,
        component_b64: &str,
        controller_b64: &str,
    ) -> Option<Vec<u8>> {
        self.host_for(instance_id)?
            .packed_state(instance_id, component_b64, controller_b64)
    }

    pub fn send_plugin_state(
        &mut self,
        instance_id: &str,
        packed: &[u8],
    ) -> Result<(), PluginHostClientError> {
        let Some(host) = self.host_for_mut(instance_id) else {
            return Err(Self::not_loaded(instance_id));
        };
        host.send_plugin_state(instance_id, packed)
    }

    pub fn request_plugin_parameters(
        &mut self,
        plugin_instance_id: &str,
    ) -> Result<(), PluginHostClientError> {
        match self.host_for_mut(plugin_instance_id) {
            Some(host) => host.request_plugin_parameters(plugin_instance_id),
            None => Ok(()),
        }
    }

    pub fn poll(&mut self) {
        for host in self.hosts.values_mut() {
            host.poll();
        }
        self.reap_dead_hosts();
    }

    /// Events from every host. A host that died is not reported here but
    /// through [`Self::take_lost_hosts`].
    pub fn drain_events(&mut self) -> Vec<ClientEvent> {
        let mut events = Vec::new();
        for host in self.hosts.values_mut() {
            events.extend(host.drain_events());
        }
        self.reap_dead_hosts();
        events
    }

    /// Hosts that died since the last call, each with what it held.
    pub fn take_lost_hosts(&mut self) -> Vec<LostBridgeHost> {
        std::mem::take(&mut self.lost)
    }

    fn reap_dead_hosts(&mut self) {
        let dead: Vec<String> = self
            .hosts
            .iter()
            .filter(|(_, host)| host.dead)
            .map(|(key, _)| key.clone())
            .collect();
        for key in dead {
            let Some(mut host) = self.hosts.remove(&key) else {
                continue;
            };
            let instances: Vec<BridgeLoadedPlugin> = host.loaded.values().cloned().collect();
            for instance in &instances {
                self.instance_host.remove(&instance.descriptor.insert_id);
            }
            eprintln!(
                "[plugin-bridge] host lost key={key} pid={:?} instances={}",
                host.host_pid,
                instances.len()
            );
            let pid = host.host_pid;
            host.abandon();
            self.lost.push(LostBridgeHost {
                key,
                pid,
                instances,
            });
        }
    }

    /// Routed by the command's own instance id; a command naming none goes
    /// to every host.
    pub fn send_raw(&mut self, command: &HostCommand) -> Result<(), PluginHostClientError> {
        let target = serde_json::to_value(command).ok().and_then(|value| {
            fn find(value: &serde_json::Value) -> Option<String> {
                match value {
                    serde_json::Value::Object(map) => map
                        .get("plugin_instance_id")
                        .and_then(|id| id.as_str().map(str::to_string))
                        .or_else(|| map.values().find_map(find)),
                    _ => None,
                }
            }
            find(&value)
        });
        match target {
            Some(instance_id) => match self.host_for_mut(&instance_id) {
                Some(host) => host.send_raw(command),
                None => Err(Self::not_loaded(&instance_id)),
            },
            None => {
                let mut result = Ok(());
                for host in self.hosts.values_mut() {
                    if let Err(error) = host.send_raw(command) {
                        result = Err(error);
                    }
                }
                result
            }
        }
    }

    pub fn builtin_meter_frame(
        &self,
        instance_id: &str,
    ) -> Option<SpherePluginHost::audio_bridge::BuiltinMeterFrame> {
        self.host_for(instance_id)?.builtin_meter_frame(instance_id)
    }

    pub fn builtin_spectrum_frame(
        &self,
        instance_id: &str,
    ) -> Option<(u32, [f32; SpherePluginHost::spectrum::SPECTRUM_BINS])> {
        self.host_for(instance_id)?
            .builtin_spectrum_frame(instance_id)
    }

    pub fn builtin_stereo_image_frame(
        &self,
        instance_id: &str,
    ) -> Option<(u32, SpherePluginHost::audio_bridge::StereoImageFrame)> {
        self.host_for(instance_id)?
            .builtin_stereo_image_frame(instance_id)
    }

    pub fn builtin_pad_levels(
        &self,
        instance_id: &str,
    ) -> Option<(
        u32,
        [f32; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
    )> {
        self.host_for(instance_id)?.builtin_pad_levels(instance_id)
    }

    pub fn builtin_host_status(&self, instance_id: &str) -> Option<(u32, u32, u32, f64)> {
        self.host_for(instance_id)?.builtin_host_status(instance_id)
    }

    /// Graceful shutdown of every host. Drains the slot so every
    /// [`PluginHostClient`] is dropped and process handles are released.
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

    if let Ok(mut pool) = runtime.lock() {
        let hosts: Vec<(String, BridgeHost)> = pool.hosts.drain().collect();
        pool.instance_host.clear();
        for (key, mut host) in hosts {
            if let Some(pid) = host.host_pid {
                progress(
                    format!("Waiting for plugin host pid={pid}"),
                    ProgressBarValue::value(0.7),
                );
            }
            let had_pid = host.host_pid.is_some();
            eprintln!("[plugin-bridge] shutdown host key={key}");
            host.shutdown(timeout);
            if had_pid {
                report.hosts_shutdown += 1;
            }
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

/// `FUTUREBOARD_PLUGIN_PREVIEW_DEBUG=1` logs every preview note sent over IPC.
/// Off by default: it runs once per played note, and a console write per note
/// is jitter on the live-input path.
fn preview_note_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_PREVIEW_DEBUG").is_some())
}
