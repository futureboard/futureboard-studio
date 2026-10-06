//! Third-party effects (VST3, VST2, CLAP, AU) in Futureboard's plug-in host.
//!
//! The same process Studio uses (`FutureboardPluginHostX64`), driven the same
//! way: one shared-memory region per insert, a one-block exchange per audio
//! callback, and the host woken by a kernel event rather than a timer. A
//! crash in a plug-in takes the host down, not the mixer: the inserts it held
//! pass their input through until they are loaded again.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use DirectAudio::plugin_bridge::SharedPluginBridgeSink;
use SpherePluginHost::audio_bridge::{BridgeKickEvent, SharedAudioRegion, bridge_kick_event_name};
use SpherePluginHost::ipc::HostEvent;
use SpherePluginHost::plugin_bridge_sink::SharedRegionSink;
use SpherePluginHost::plugin_host_client::{ClientEvent, PluginHostClient};

use crate::graph::MAX_BLOCK;
use crate::session::Id;

/// The audio-thread half of a bridged insert.
pub struct ExternalInsert {
    sink: SharedPluginBridgeSink,
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
}

impl ExternalInsert {
    /// Exchange one block with the host, in place. The host's answer is one
    /// block late; a block it has not answered yet passes through dry, and a
    /// stale answer is never replayed.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len()).min(self.scratch_l.len());
        if n == 0 {
            return;
        }
        // Order matters (see DirectAudio's `apply_external_bridge_insert_block`):
        // read the previous wet block first — that proves the host has
        // released the input buffer — then write the next dry one, then ask.
        let got = self
            .sink
            .read_output(&mut self.scratch_l[..n], &mut self.scratch_r[..n], n);
        let can_publish = got > 0 || !self.sink.request_in_flight();
        if can_publish {
            self.sink.write_input(&left[..n], &right[..n], n);
        }
        if got > 0 {
            let got = got.min(n);
            left[..got].copy_from_slice(&self.scratch_l[..got]);
            right[..got].copy_from_slice(&self.scratch_r[..got]);
        }
        if can_publish {
            self.sink.request_block(n as u32);
        }
    }
}

/// One installed third-party effect, from the catalog Studio's plug-in
/// scanner keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledEffect {
    pub name: String,
    pub vendor: Option<String>,
    /// "VST3", "VST2", "CLAP" or "AU".
    pub format: String,
    pub path: String,
    pub class_id: String,
}

/// Every usable installed effect, sorted by name. Empty when the catalog does
/// not exist yet (Studio has never scanned on this machine) — LiveStage reads
/// it but does not scan.
pub fn installed_effects() -> Vec<InstalledEffect> {
    use SpherePluginHost::registry::PluginFormat;
    let Ok(conn) = SpherePluginHost::plugin_db::open_database_readonly() else {
        return Vec::new();
    };
    let Ok(rows) = SpherePluginHost::plugin_db::read_all(&conn) else {
        return Vec::new();
    };
    let mut effects: Vec<InstalledEffect> = rows
        .into_iter()
        .filter(|row| row.is_effect && !row.disabled && row.scan_status.is_usable())
        .filter(|row| {
            matches!(
                row.format,
                PluginFormat::Vst3 | PluginFormat::Vst2 | PluginFormat::Clap | PluginFormat::Au
            )
        })
        .filter_map(|row| {
            Some(InstalledEffect {
                name: row.name,
                vendor: row.vendor,
                format: row.format.label().to_string(),
                path: row.path.to_string_lossy().to_string(),
                class_id: row.class_id.filter(|id| !id.is_empty())?,
            })
        })
        .filter(|effect| !effect.path.is_empty())
        .collect();
    effects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    effects.dedup_by(|a, b| a.path == b.path && a.class_id == b.class_id);
    effects
}

/// What the plug-in host told the engine since the last poll.
#[derive(Debug, Clone)]
pub enum ExternalEvent {
    Loaded {
        insert: Id,
        name: String,
    },
    LoadFailed {
        insert: Id,
        error: String,
    },
    EditorAttached {
        insert: Id,
        width: u32,
        height: u32,
    },
    EditorResize {
        insert: Id,
        width: u32,
        height: u32,
    },
    EditorClosed {
        insert: Id,
    },
    State {
        insert: Id,
        component: String,
        controller: String,
    },
    /// The host process went away; every external insert is passing through.
    HostLost,
}

/// The plug-in host process and the inserts it runs.
pub struct ExternalHost {
    client: PluginHostClient,
    kick: Option<Arc<BridgeKickEvent>>,
    host_alive: Arc<AtomicBool>,
    regions: HashMap<Id, Arc<SharedAudioRegion>>,
    configured: Option<(u32, u32)>,
    lost: bool,
}

fn instance_name(insert: Id) -> String {
    format!("livestage-{insert}")
}

fn insert_of(instance: &str) -> Option<Id> {
    instance.strip_prefix("livestage-")?.parse().ok()
}

fn region_name(instance: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "Local\\FutureboardLiveStage-{}__{}__g{}",
        std::process::id(),
        instance,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

impl ExternalHost {
    pub fn spawn() -> Result<Self, String> {
        let mut client = PluginHostClient::spawn_bridge().map_err(|error| error.to_string())?;
        let _ = client.ping();
        let kick = BridgeKickEvent::create_named(&bridge_kick_event_name(
            std::process::id(),
            client.pid(),
        ))
        .ok()
        .map(Arc::new);
        Ok(Self {
            client,
            kick,
            host_alive: Arc::new(AtomicBool::new(true)),
            regions: HashMap::new(),
            configured: None,
            lost: false,
        })
    }

    pub fn is_alive(&self) -> bool {
        !self.lost
    }

    /// Load a plug-in for `insert` and hand back its audio-thread half. The
    /// host confirms (or refuses) asynchronously: see [`Self::poll`]. Until it
    /// answers, the insert passes its input through.
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        &mut self,
        insert: Id,
        format: &str,
        path: &str,
        class_id: &str,
        sample_rate: u32,
        state: Option<&(String, String)>,
    ) -> Result<ExternalInsert, String> {
        let block = MAX_BLOCK as u32;
        if self.configured != Some((sample_rate, block)) {
            self.client
                .configure_audio_bridge(sample_rate, block)
                .map_err(|error| error.to_string())?;
            self.configured = Some((sample_rate, block));
        }
        let instance = instance_name(insert);
        let name = region_name(&instance);
        let region = SharedAudioRegion::create_named(&name, sample_rate, block, 2)
            .map_err(|error| format!("shared audio region: {error}"))?;
        self.client
            .attach_shared_audio(name, region.bytes(), instance.clone())
            .map_err(|error| error.to_string())?;
        self.client
            .load_plugin(
                instance.clone(),
                path.to_string(),
                class_id.to_string(),
                sample_rate,
                block,
                Some(format.to_string()),
            )
            .map_err(|error| error.to_string())?;
        self.client
            .prepare_processing(instance.clone(), sample_rate, block, 2, 2)
            .map_err(|error| error.to_string())?;
        if let Some((component, controller)) = state {
            let _ = self
                .client
                .set_plugin_state(instance, component.clone(), controller.clone());
        }
        let region = Arc::new(region);
        self.regions.insert(insert, region.clone());
        let sink = SharedRegionSink::into_shared_with_liveness(
            region,
            self.kick.clone(),
            self.host_alive.clone(),
        );
        Ok(ExternalInsert {
            sink,
            scratch_l: vec![0.0; MAX_BLOCK],
            scratch_r: vec![0.0; MAX_BLOCK],
        })
    }

    /// Release `insert`'s plug-in. Its region stays mapped until the last
    /// graph holding the insert is dropped.
    pub fn unload(&mut self, insert: Id) {
        let _ = self.client.unload_plugin(instance_name(insert));
        self.regions.remove(&insert);
    }

    /// Attach `insert`'s editor into the native window `parent` (an HWND on
    /// Windows, an X11 window on Linux).
    #[allow(clippy::too_many_arguments)]
    pub fn open_editor(
        &mut self,
        insert: Id,
        path: &str,
        class_id: &str,
        parent: u64,
        width: u32,
        height: u32,
        dpi: u32,
    ) -> Result<(), String> {
        self.client
            .open_editor(
                instance_name(insert),
                path.to_string(),
                class_id.to_string(),
                parent,
                width,
                height,
                dpi,
            )
            .map_err(|error| error.to_string())
    }

    pub fn resize_editor(&mut self, insert: Id, width: u32, height: u32, dpi: u32) {
        let _ = self
            .client
            .resize_editor(instance_name(insert), width, height, dpi);
    }

    pub fn close_editor(&mut self, insert: Id) {
        let _ = self.client.close_editor(instance_name(insert));
    }

    /// Ask for `insert`'s state; it arrives as [`ExternalEvent::State`].
    pub fn request_state(&mut self, insert: Id) {
        let _ = self.client.get_plugin_state(instance_name(insert));
    }

    /// Everything the host reported since the last call.
    pub fn poll(&mut self) -> Vec<ExternalEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.client.try_recv_event() {
            match event {
                ClientEvent::Disconnected => {
                    if !self.lost {
                        self.lost = true;
                        self.host_alive.store(false, Ordering::Relaxed);
                        events.push(ExternalEvent::HostLost);
                    }
                }
                ClientEvent::Host(event) => {
                    if let Some(event) = translate(event) {
                        events.push(event);
                    }
                }
            }
        }
        events
    }
}

fn translate(event: HostEvent) -> Option<ExternalEvent> {
    Some(match event {
        HostEvent::PluginLoaded {
            plugin_instance_id,
            name,
        }
        | HostEvent::PluginAlreadyLoaded {
            plugin_instance_id,
            name,
        } => ExternalEvent::Loaded {
            insert: insert_of(&plugin_instance_id)?,
            name,
        },
        HostEvent::PluginLoadFailed {
            plugin_instance_id,
            error,
        } => ExternalEvent::LoadFailed {
            insert: insert_of(&plugin_instance_id)?,
            error,
        },
        HostEvent::EditorAttached {
            plugin_instance_id,
            preferred_width,
            preferred_height,
            ..
        } => ExternalEvent::EditorAttached {
            insert: insert_of(&plugin_instance_id)?,
            width: preferred_width,
            height: preferred_height,
        },
        HostEvent::EditorContentResize {
            plugin_instance_id,
            width,
            height,
            ..
        } => ExternalEvent::EditorResize {
            insert: insert_of(&plugin_instance_id)?,
            width,
            height,
        },
        HostEvent::EditorClosed { plugin_instance_id } => ExternalEvent::EditorClosed {
            insert: insert_of(&plugin_instance_id)?,
        },
        HostEvent::PluginState {
            plugin_instance_id,
            ok: true,
            component_b64,
            controller_b64,
        } => ExternalEvent::State {
            insert: insert_of(&plugin_instance_id)?,
            component: component_b64,
            controller: controller_b64,
        },
        _ => return None,
    })
}

impl Drop for ExternalHost {
    fn drop(&mut self) {
        self.host_alive.store(false, Ordering::Relaxed);
        let _ = self.client.shutdown();
    }
}
