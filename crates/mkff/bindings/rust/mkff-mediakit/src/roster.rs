//! The media roster (BeOS `BMediaRoster`): the registry of add-ons, accelerants and running
//! nodes, the connection table, and codec selection.
//!
//! BeOS kept this state in a separate `media_server` process; MKFF keeps it in-process (one
//! [`MediaRoster::roster`] per process, or private rosters for embedding and tests), but the
//! shape is the same: nodes are registered with the roster, connected output → input after
//! format negotiation, slaved to a time source and started / stopped / seeked at performance
//! times; codecs are found by asking the roster for a flavor that turns one format into another.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use crate::accelerant::{self, Accelerant};
use crate::addon::{
    AudioDecoderPlugin, AudioEncoderPlugin, FlavorInfo, Instance, Kinds, MediaAddOn,
    VideoDecoderPlugin, VideoEncoderPlugin,
};
use crate::format::MediaFormat;
use crate::node::{
    MediaDestination, MediaEvent, MediaInput, MediaNode, MediaOutput, MediaSource, NodeHandle,
    NodeId, IMMEDIATE,
};
use crate::time_source::TimeSource;
use crate::{Error, Result};

/// How long roster → node requests wait for the node's thread.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Which flavors codec selection may use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccelPolicy {
    /// Hardware first, software when no hardware flavor accepts the stream or it fails to open.
    #[default]
    Auto,
    HardwareOnly,
    SoftwareOnly,
}

impl AccelPolicy {
    /// From the `MKFF_ACCEL` environment variable (`auto`, `hw` / `hardware`, `sw` / `software` /
    /// `off`), defaulting to `Auto`.
    pub fn from_env() -> AccelPolicy {
        match std::env::var("MKFF_ACCEL")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "hw" | "hardware" | "gpu" => AccelPolicy::HardwareOnly,
            "sw" | "software" | "off" | "cpu" | "0" => AccelPolicy::SoftwareOnly,
            _ => AccelPolicy::Auto,
        }
    }
}

/// Identifies a registered add-on.
pub type AddOnId = u32;

/// A flavor of a registered add-on (`dormant_node_info`).
#[derive(Clone, Debug)]
pub struct FlavorRef {
    pub addon: AddOnId,
    pub addon_name: String,
    pub info: FlavorInfo,
}

/// A registered node (`media_node`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRef {
    pub id: NodeId,
    pub name: String,
    pub kinds: Kinds,
}

/// A live connection (`media_output` + `media_input` pair).
#[derive(Clone, Debug, PartialEq)]
pub struct Connection {
    pub source: MediaSource,
    pub destination: MediaDestination,
    pub format: MediaFormat,
}

pub(crate) struct RosterInner {
    addons: RwLock<Vec<(AddOnId, Arc<dyn MediaAddOn>)>>,
    nodes: RwLock<HashMap<NodeId, Arc<NodeHandle>>>,
    connections: RwLock<HashMap<MediaSource, Connection>>,
    next_id: AtomicU32,
    time_source: TimeSource,
    accelerants: OnceLock<Vec<Arc<dyn Accelerant>>>,
}

impl RosterInner {
    pub(crate) fn destination_of(&self, src: MediaSource) -> Option<MediaDestination> {
        self.connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&src)
            .map(|c| c.destination)
    }

    pub(crate) fn post(&self, node: NodeId, time: i64, event: MediaEvent) -> Result<()> {
        let h = self.handle(node)?;
        h.post(time, event);
        Ok(())
    }

    fn handle(&self, node: NodeId) -> Result<Arc<NodeHandle>> {
        self.nodes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&node)
            .cloned()
            .ok_or_else(|| Error::BadNode(format!("no node {node}")))
    }
}

impl Drop for RosterInner {
    fn drop(&mut self) {
        let nodes: Vec<_> = self
            .nodes
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in nodes {
            h.quit();
        }
    }
}

/// The media roster. Cloning gives another handle to the same roster.
#[derive(Clone)]
pub struct MediaRoster {
    inner: Arc<RosterInner>,
}

static ROSTER: OnceLock<MediaRoster> = OnceLock::new();

impl Default for MediaRoster {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaRoster {
    /// The process-wide roster (`BMediaRoster::Roster()`), with the platform's accelerant add-on
    /// (hardware video decode / encode) registered. Software codec crates register their own
    /// add-ons into it (e.g. `mkff_audio::register(MediaRoster::roster())`).
    pub fn roster() -> &'static MediaRoster {
        ROSTER.get_or_init(|| {
            let r = MediaRoster::new();
            accelerant::register_platform_addons(&r);
            r
        })
    }

    /// An empty private roster (no add-ons registered).
    pub fn new() -> MediaRoster {
        MediaRoster {
            inner: Arc::new(RosterInner {
                addons: RwLock::new(Vec::new()),
                nodes: RwLock::new(HashMap::new()),
                connections: RwLock::new(HashMap::new()),
                next_id: AtomicU32::new(1),
                time_source: TimeSource::system(),
                accelerants: OnceLock::new(),
            }),
        }
    }

    // ---- add-ons ---------------------------------------------------------------------------

    pub fn register_addon(&self, addon: Arc<dyn MediaAddOn>) -> AddOnId {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        log::debug!("media add-on {id}: {}", addon.name());
        self.inner
            .addons
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, addon));
        id
    }

    pub fn unregister_addon(&self, id: AddOnId) {
        self.inner
            .addons
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(i, _)| *i != id);
    }

    /// Registered add-ons by name.
    pub fn addons(&self) -> Vec<(AddOnId, String)> {
        self.inner
            .addons
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(i, a)| (*i, a.name().to_owned()))
            .collect()
    }

    /// Every flavor of every add-on (`GetDormantNodes` with no filter).
    pub fn flavors(&self) -> Vec<FlavorRef> {
        let addons = self
            .inner
            .addons
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut out = Vec::new();
        for (id, a) in addons {
            for info in a.flavors() {
                out.push(FlavorRef {
                    addon: id,
                    addon_name: a.name().to_owned(),
                    info,
                });
            }
        }
        out
    }

    /// Flavors of kind `kinds` that accept `input` and can produce `output`, best first under
    /// `policy` (`GetDormantNodes` with formats).
    pub fn find_flavors(
        &self,
        kinds: Kinds,
        input: &MediaFormat,
        output: &MediaFormat,
        policy: AccelPolicy,
    ) -> Vec<FlavorRef> {
        let mut v: Vec<FlavorRef> = self
            .flavors()
            .into_iter()
            .filter(|f| f.info.kinds.contains(kinds))
            .filter(|f| f.info.in_formats.iter().any(|m| m.matches(input)))
            .filter(|f| {
                f.info.out_formats.is_empty()
                    || f.info.out_formats.iter().any(|m| m.matches(output))
            })
            .filter(|f| match policy {
                AccelPolicy::Auto => true,
                AccelPolicy::HardwareOnly => f.info.accel.is_hardware(),
                AccelPolicy::SoftwareOnly => !f.info.accel.is_hardware(),
            })
            .collect();
        v.sort_by(|a, b| {
            b.info
                .accel
                .is_hardware()
                .cmp(&a.info.accel.is_hardware())
                .then(b.info.rank.cmp(&a.info.rank))
        });
        v
    }

    /// Instantiate a flavor (`InstantiateDormantNode`).
    pub fn instantiate(
        &self,
        flavor: &FlavorRef,
        input: &MediaFormat,
        output: &MediaFormat,
    ) -> Result<Instance> {
        let addon = self
            .inner
            .addons
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(i, _)| *i == flavor.addon)
            .map(|(_, a)| Arc::clone(a))
            .ok_or_else(|| Error::BadNode(format!("add-on {} is gone", flavor.addon)))?;
        addon.instantiate(flavor.info.id, input, output)
    }

    /// Try candidates in order until one instantiates.
    fn first_instance<T>(
        &self,
        kinds: Kinds,
        input: &MediaFormat,
        output: &MediaFormat,
        policy: AccelPolicy,
        pick: impl Fn(Instance) -> Option<T>,
    ) -> Result<(FlavorRef, T)> {
        let candidates = self.find_flavors(kinds, input, output, policy);
        if candidates.is_empty() {
            return Err(Error::Unsupported(format!(
                "no {} for {input} (policy {policy:?})",
                kind_name(kinds)
            )));
        }
        let mut last = None;
        for f in candidates {
            match self.instantiate(&f, input, output) {
                Ok(inst) => match pick(inst) {
                    Some(t) => {
                        log::debug!(
                            "{} for {input}: {} / {} ({})",
                            kind_name(kinds),
                            f.addon_name,
                            f.info.name,
                            f.info.accel
                        );
                        return Ok((f, t));
                    }
                    None => {
                        last = Some(Error::BadFormat(format!(
                            "{} returned the wrong kind of instance",
                            f.info.name
                        )))
                    }
                },
                Err(e) => {
                    log::debug!("{} / {} declined {input}: {e}", f.addon_name, f.info.name);
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| Error::Unsupported(input.to_string())))
    }

    /// The best audio decoder for `format` (an encoded audio format with its configuration).
    pub fn audio_decoder(
        &self,
        format: &MediaFormat,
        policy: AccelPolicy,
    ) -> Result<(FlavorRef, Box<dyn AudioDecoderPlugin>)> {
        self.first_instance(
            Kinds::DECODER,
            format,
            &MediaFormat::Wildcard,
            policy,
            |i| match i {
                Instance::AudioDecoder(d) => Some(d),
                _ => None,
            },
        )
    }

    /// The best video decoder for `format`: the GPU's when an accelerant takes the stream.
    pub fn video_decoder(
        &self,
        format: &MediaFormat,
        policy: AccelPolicy,
    ) -> Result<(FlavorRef, Box<dyn VideoDecoderPlugin>)> {
        self.first_instance(
            Kinds::DECODER,
            format,
            &MediaFormat::Wildcard,
            policy,
            |i| match i {
                Instance::VideoDecoder(d) => Some(d),
                _ => None,
            },
        )
    }

    /// An audio encoder from `input` (raw audio) to `output` (encoded audio: codec, rate,
    /// channels, bit rate).
    pub fn audio_encoder(
        &self,
        input: &MediaFormat,
        output: &MediaFormat,
        policy: AccelPolicy,
    ) -> Result<(FlavorRef, Box<dyn AudioEncoderPlugin>)> {
        self.first_instance(Kinds::ENCODER, input, output, policy, |i| match i {
            Instance::AudioEncoder(e) => Some(e),
            _ => None,
        })
    }

    /// A video encoder from `input` (raw video) to `output` (encoded video).
    pub fn video_encoder(
        &self,
        input: &MediaFormat,
        output: &MediaFormat,
        policy: AccelPolicy,
    ) -> Result<(FlavorRef, Box<dyn VideoEncoderPlugin>)> {
        self.first_instance(Kinds::ENCODER, input, output, policy, |i| match i {
            Instance::VideoEncoder(e) => Some(e),
            _ => None,
        })
    }

    // ---- accelerants -----------------------------------------------------------------------

    /// The GPUs' accelerants (enumerated once).
    pub fn accelerants(&self) -> &[Arc<dyn Accelerant>] {
        self.inner.accelerants.get_or_init(accelerant::enumerate)
    }

    // ---- nodes -----------------------------------------------------------------------------

    /// The default time source nodes are slaved to.
    pub fn time_source(&self) -> &TimeSource {
        &self.inner.time_source
    }

    /// Register a node: it gets an id and its own event-looper thread (`RegisterNode`).
    pub fn register_node(&self, node: Box<dyn MediaNode>) -> Result<NodeRef> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let handle = NodeHandle::spawn(
            id,
            node,
            Arc::downgrade(&self.inner),
            self.inner.time_source.clone(),
        )?;
        let r = NodeRef {
            id,
            name: handle.name.clone(),
            kinds: handle.kinds,
        };
        self.inner
            .nodes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, handle);
        Ok(r)
    }

    /// Instantiate a node flavor and register it.
    pub fn instantiate_node(
        &self,
        flavor: &FlavorRef,
        input: &MediaFormat,
        output: &MediaFormat,
    ) -> Result<NodeRef> {
        match self.instantiate(flavor, input, output)? {
            Instance::Node(n) => self.register_node(n),
            other => Err(Error::BadNode(format!(
                "{} is not a node flavor ({other:?})",
                flavor.info.name
            ))),
        }
    }

    /// Disconnect everything on `node` and stop its thread (`ReleaseNode`).
    pub fn release_node(&self, node: NodeId) -> Result<()> {
        let conns: Vec<Connection> = self
            .inner
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|c| c.source.node == node || c.destination.node == node)
            .cloned()
            .collect();
        for c in conns {
            let _ = self.disconnect(c.source, c.destination);
        }
        let h = self
            .inner
            .nodes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&node);
        match h {
            Some(h) => {
                h.quit();
                Ok(())
            }
            None => Err(Error::BadNode(format!("no node {node}"))),
        }
    }

    pub fn nodes(&self) -> Vec<NodeRef> {
        let mut v: Vec<NodeRef> = self
            .inner
            .nodes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|h| NodeRef {
                id: h.id,
                name: h.name.clone(),
                kinds: h.kinds,
            })
            .collect();
        v.sort_by_key(|n| n.id);
        v
    }

    /// Outputs of `node` not yet connected (`GetFreeOutputsFor`).
    pub fn free_outputs(&self, node: NodeId) -> Result<Vec<MediaOutput>> {
        let h = self.inner.handle(node)?;
        let mut outs = h.call(CALL_TIMEOUT, |n, _| n.outputs())?;
        let conns = self
            .inner
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner());
        outs.retain_mut(|o| {
            o.source.node = node;
            !conns.contains_key(&o.source)
        });
        Ok(outs)
    }

    /// Inputs of `node` not yet connected (`GetFreeInputsFor`).
    pub fn free_inputs(&self, node: NodeId) -> Result<Vec<MediaInput>> {
        let h = self.inner.handle(node)?;
        let mut ins = h.call(CALL_TIMEOUT, |n, _| n.inputs())?;
        let conns = self
            .inner
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner());
        ins.retain_mut(|i| {
            i.destination.node = node;
            !conns.values().any(|c| c.destination == i.destination)
        });
        Ok(ins)
    }

    /// Connect `source` to `destination`, negotiating from `format` (`Connect`): the producer
    /// proposes, the consumer accepts, the producer prepares, then both sides are told. Returns
    /// the agreed format.
    pub fn connect(
        &self,
        source: MediaSource,
        destination: MediaDestination,
        format: &MediaFormat,
    ) -> Result<MediaFormat> {
        if self.inner.destination_of(source).is_some() {
            return Err(Error::BadNode(format!(
                "output {source:?} is already connected"
            )));
        }
        let producer = self.inner.handle(source.node)?;
        let consumer = self.inner.handle(destination.node)?;
        let req = format.clone();
        let proposal = producer.call(CALL_TIMEOUT, move |n, _| {
            n.format_proposal(source.port, &req)
        })??;
        if !proposal.matches(format) {
            return Err(Error::BadFormat(format!(
                "{} proposed {proposal}, wanted {format}",
                producer.name
            )));
        }
        let p = proposal.clone();
        let accepted = consumer.call(CALL_TIMEOUT, move |n, _| {
            n.accept_format(destination.port, &p)
        })??;
        if !accepted.matches(&proposal) {
            return Err(Error::BadFormat(format!(
                "{} accepted {accepted} for {proposal}",
                consumer.name
            )));
        }
        let mut agreed = accepted.clone();
        agreed.specialize(&proposal);
        let a = agreed.clone();
        let agreed = producer.call(CALL_TIMEOUT, move |n, _| {
            n.prepare_to_connect(source.port, destination, &a)
        })??;
        // record the connection before telling the nodes, so buffers sent from `connect` route
        self.inner
            .connections
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                source,
                Connection {
                    source,
                    destination,
                    format: agreed.clone(),
                },
            );
        let a = agreed.clone();
        let r = consumer
            .call(CALL_TIMEOUT, move |n, c| {
                n.connected(destination.port, source, &a, c)
            })
            .and_then(|r| r);
        let r = r.and_then(|_| {
            let a = agreed.clone();
            producer
                .call(CALL_TIMEOUT, move |n, c| {
                    n.connect(source.port, destination, &a, c)
                })
                .and_then(|r| r)
        });
        if let Err(e) = r {
            self.inner
                .connections
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&source);
            return Err(e);
        }
        log::debug!("connected {} → {}: {agreed}", producer.name, consumer.name);
        Ok(agreed)
    }

    /// Connect the first free output of `producer` to the first free input of `consumer` that
    /// agree on a format.
    pub fn connect_nodes(&self, producer: NodeId, consumer: NodeId) -> Result<Connection> {
        let outs = self.free_outputs(producer)?;
        let ins = self.free_inputs(consumer)?;
        let mut last = Error::BadFormat("no free output / input pair".into());
        for o in &outs {
            for i in &ins {
                if !o.format.matches(&i.format) {
                    continue;
                }
                let mut f = o.format.clone();
                f.specialize(&i.format);
                match self.connect(o.source, i.destination, &f) {
                    Ok(format) => {
                        return Ok(Connection {
                            source: o.source,
                            destination: i.destination,
                            format,
                        })
                    }
                    Err(e) => last = e,
                }
            }
        }
        Err(last)
    }

    pub fn disconnect(&self, source: MediaSource, destination: MediaDestination) -> Result<()> {
        let removed = self
            .inner
            .connections
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&source);
        if removed.as_ref().map(|c| c.destination) != Some(destination) {
            if let Some(c) = removed {
                self.inner
                    .connections
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(source, c);
            }
            return Err(Error::BadNode(format!(
                "{source:?} is not connected to {destination:?}"
            )));
        }
        if let Ok(p) = self.inner.handle(source.node) {
            p.control(Box::new(move |n, c| {
                n.disconnect_output(source.port, destination, c)
            }));
        }
        if let Ok(c) = self.inner.handle(destination.node) {
            c.control(Box::new(move |n, ctx| {
                n.disconnected(destination.port, source, ctx)
            }));
        }
        Ok(())
    }

    pub fn connections(&self) -> Vec<Connection> {
        self.inner
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Start `node` at performance time `at` (`StartNode`; [`IMMEDIATE`] for now).
    pub fn start_node(&self, node: NodeId, at: i64) -> Result<()> {
        self.inner.post(node, at, MediaEvent::Start)
    }

    /// Stop `node` at performance time `at` (`StopNode`).
    pub fn stop_node(&self, node: NodeId, at: i64) -> Result<()> {
        self.inner.post(node, at, MediaEvent::Stop)
    }

    /// Make `node` continue from `media_time` at performance time `at` (`SeekNode`).
    pub fn seek_node(&self, node: NodeId, media_time: i64, at: i64) -> Result<()> {
        self.inner.post(node, at, MediaEvent::Seek { media_time })
    }

    /// Post an application event to `node`.
    pub fn post_event(
        &self,
        node: NodeId,
        at: i64,
        what: u32,
        data: Box<dyn std::any::Any + Send>,
    ) -> Result<()> {
        self.inner.post(node, at, MediaEvent::User { what, data })
    }

    /// Slave `node` to `ts` (`SetTimeSourceFor`).
    pub fn set_time_source_for(&self, node: NodeId, ts: TimeSource) -> Result<()> {
        self.inner.handle(node)?.set_time_source(ts);
        Ok(())
    }

    /// Start a whole chain "now plus `lead`" on the roster's time source, so every node starts at
    /// the same performance time.
    pub fn start_all(&self, nodes: &[NodeId], lead: Duration) -> Result<i64> {
        let at = self.inner.time_source.now() + lead.as_micros() as i64;
        for &n in nodes {
            self.start_node(n, at)?;
        }
        Ok(at)
    }

    /// Run `f` on `node`'s thread (for application-defined control).
    pub fn with_node<R: Send + 'static>(
        &self,
        node: NodeId,
        f: impl FnOnce(&mut dyn MediaNode) -> R + Send + 'static,
    ) -> Result<R> {
        self.inner
            .handle(node)?
            .call(CALL_TIMEOUT, move |n, _| f(n))
    }
}

impl std::fmt::Debug for MediaRoster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaRoster")
            .field("addons", &self.addons())
            .field("nodes", &self.nodes())
            .finish()
    }
}

fn kind_name(k: Kinds) -> &'static str {
    if k.contains(Kinds::DECODER) {
        "decoder"
    } else if k.contains(Kinds::ENCODER) {
        "encoder"
    } else {
        "node"
    }
}

/// Re-exported for nodes that start "now".
pub const NOW: i64 = IMMEDIATE;
