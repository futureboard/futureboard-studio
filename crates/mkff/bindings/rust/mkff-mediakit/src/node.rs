//! Media nodes (BeOS `BMediaNode` / `BBufferProducer` / `BBufferConsumer` /
//! `BMediaEventLooper`).
//!
//! Every registered node runs on its own thread with a timed event queue, like a BeOS node's
//! control port plus `BTimedEventQueue`. Events are dispatched when their performance time comes
//! up on the node's [`TimeSource`] (minus the node's scheduling latency); control requests from
//! the roster (format negotiation, connect, …) run on the same thread in between, so a node never
//! needs locks of its own.

use std::any::Any;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::addon::Kinds;
use crate::buffer::Buffer;
use crate::format::MediaFormat;
use crate::roster::RosterInner;
use crate::time_source::TimeSource;
use crate::{Error, Result};

pub type NodeId = u32;

/// An output port of a producer (`media_source`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaSource {
    pub node: NodeId,
    pub port: u32,
}

/// An input port of a consumer (`media_destination`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaDestination {
    pub node: NodeId,
    pub port: u32,
}

/// A producer's output as it describes it (`media_output`). `source.node` is filled in by the
/// roster; nodes only set `source.port`.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaOutput {
    pub source: MediaSource,
    pub destination: Option<MediaDestination>,
    pub format: MediaFormat,
    pub name: String,
}

/// A consumer's input (`media_input`).
#[derive(Clone, Debug, PartialEq)]
pub struct MediaInput {
    pub destination: MediaDestination,
    pub source: Option<MediaSource>,
    pub format: MediaFormat,
    pub name: String,
}

/// Run state of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RunState {
    #[default]
    Stopped,
    Started,
}

/// Producer-side data status sent to consumers (`SendDataStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataStatus {
    Available,
    NotAvailable,
    EndOfStream,
}

/// Events in a node's queue (`media_timed_event`).
pub enum MediaEvent {
    Start,
    Stop,
    /// Media time to continue from.
    Seek {
        media_time: i64,
    },
    /// A buffer arrived on input `port`.
    Buffer {
        port: u32,
        buffer: Buffer,
    },
    DataStatus {
        port: u32,
        status: DataStatus,
    },
    /// Application-defined.
    User {
        what: u32,
        data: Box<dyn Any + Send>,
    },
}

impl std::fmt::Debug for MediaEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaEvent::Start => write!(f, "Start"),
            MediaEvent::Stop => write!(f, "Stop"),
            MediaEvent::Seek { media_time } => write!(f, "Seek({media_time})"),
            MediaEvent::Buffer { port, buffer } => {
                write!(f, "Buffer(port {port}, {} bytes)", buffer.header.size_used)
            }
            MediaEvent::DataStatus { port, status } => {
                write!(f, "DataStatus(port {port}, {status:?})")
            }
            MediaEvent::User { what, .. } => write!(f, "User({what})"),
        }
    }
}

/// Performance time meaning "as soon as possible".
pub const IMMEDIATE: i64 = i64::MIN;

/// A media node. Implement the producer and / or consumer hooks that apply; the defaults refuse.
pub trait MediaNode: Send + 'static {
    fn name(&self) -> String;
    fn kinds(&self) -> Kinds;

    /// Called on the node's thread after registration.
    fn node_registered(&mut self, _ctx: &mut NodeContext) {}

    /// Handle a timed event (`HandleEvent`). `lateness` is how late (µs) the event is being
    /// handled relative to its performance time; buffers that are too late may be dropped.
    fn handle_event(&mut self, event: MediaEvent, time: i64, lateness: i64, ctx: &mut NodeContext);

    /// Processing + downstream latency (`GetLatency`): buffers are dispatched this much before
    /// their performance time.
    fn latency(&self) -> Duration {
        Duration::ZERO
    }

    // ---- producer --------------------------------------------------------------------------

    /// Free and connected outputs. Only `source.port` needs to be set.
    fn outputs(&self) -> Vec<MediaOutput> {
        Vec::new()
    }

    /// Propose a format for output `port`, specialising the request (`FormatProposal`).
    fn format_proposal(&mut self, _port: u32, _request: &MediaFormat) -> Result<MediaFormat> {
        Err(Error::Unsupported(format!(
            "{} produces nothing",
            self.name()
        )))
    }

    /// Final chance to change the format before connecting (`PrepareToConnect`).
    fn prepare_to_connect(
        &mut self,
        _port: u32,
        _dest: MediaDestination,
        format: &MediaFormat,
    ) -> Result<MediaFormat> {
        Ok(format.clone())
    }

    /// Output `port` is now connected to `dest` with `format` (`Connect`).
    fn connect(
        &mut self,
        _port: u32,
        _dest: MediaDestination,
        _format: &MediaFormat,
        _ctx: &mut NodeContext,
    ) -> Result<()> {
        Ok(())
    }

    fn disconnect_output(&mut self, _port: u32, _dest: MediaDestination, _ctx: &mut NodeContext) {}

    // ---- consumer --------------------------------------------------------------------------

    fn inputs(&self) -> Vec<MediaInput> {
        Vec::new()
    }

    /// Accept (and specialise) a format for input `port` (`AcceptFormat`).
    fn accept_format(&mut self, _port: u32, _format: &MediaFormat) -> Result<MediaFormat> {
        Err(Error::Unsupported(format!(
            "{} consumes nothing",
            self.name()
        )))
    }

    /// Input `port` is now connected from `source` (`Connected`).
    fn connected(
        &mut self,
        _port: u32,
        _source: MediaSource,
        _format: &MediaFormat,
        _ctx: &mut NodeContext,
    ) -> Result<()> {
        Ok(())
    }

    fn disconnected(&mut self, _port: u32, _source: MediaSource, _ctx: &mut NodeContext) {}
}

/// What a node can reach from its thread.
pub struct NodeContext {
    id: NodeId,
    roster: Weak<RosterInner>,
    pub(crate) time_source: TimeSource,
    pub(crate) run_state: RunState,
}

impl NodeContext {
    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn time_source(&self) -> &TimeSource {
        &self.time_source
    }

    /// Current performance time (µs).
    pub fn now(&self) -> i64 {
        self.time_source.now()
    }

    pub fn run_state(&self) -> RunState {
        self.run_state
    }

    /// Send `buffer` out of output `port` to whatever it is connected to (`SendBuffer`). The
    /// consumer handles it at `buffer.header.start_time`.
    pub fn send_buffer(&self, port: u32, buffer: Buffer) -> Result<()> {
        let roster = self
            .roster
            .upgrade()
            .ok_or_else(|| Error::BadNode("roster gone".into()))?;
        let src = MediaSource {
            node: self.id,
            port,
        };
        let dest = roster.destination_of(src).ok_or_else(|| {
            Error::BadNode(format!(
                "output {port} of node {} is not connected",
                self.id
            ))
        })?;
        let time = buffer.header.start_time;
        roster.post(
            dest.node,
            time,
            MediaEvent::Buffer {
                port: dest.port,
                buffer,
            },
        )
    }

    /// Tell the consumer of output `port` about data availability (`SendDataStatus`).
    pub fn send_data_status(&self, port: u32, status: DataStatus, at: i64) -> Result<()> {
        let roster = self
            .roster
            .upgrade()
            .ok_or_else(|| Error::BadNode("roster gone".into()))?;
        let src = MediaSource {
            node: self.id,
            port,
        };
        let Some(dest) = roster.destination_of(src) else {
            return Ok(());
        };
        roster.post(
            dest.node,
            at,
            MediaEvent::DataStatus {
                port: dest.port,
                status,
            },
        )
    }

    /// Queue an event for this node itself (e.g. a producer's next "make a buffer" tick).
    pub fn post_self(&self, time: i64, event: MediaEvent) -> Result<()> {
        let roster = self
            .roster
            .upgrade()
            .ok_or_else(|| Error::BadNode("roster gone".into()))?;
        roster.post(self.id, time, event)
    }
}

// ---- event looper ---------------------------------------------------------------------------

struct Entry {
    time: i64,
    seq: u64,
    event: MediaEvent,
}

impl PartialEq for Entry {
    fn eq(&self, o: &Self) -> bool {
        self.time == o.time && self.seq == o.seq
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Entry {
    // BinaryHeap is a max-heap: earliest time (then first queued) must compare greatest.
    fn cmp(&self, o: &Self) -> Ordering {
        o.time.cmp(&self.time).then(o.seq.cmp(&self.seq))
    }
}

pub(crate) type Control = Box<dyn FnOnce(&mut dyn MediaNode, &mut NodeContext) + Send>;

#[derive(Default)]
struct LoopState {
    queue: BinaryHeap<Entry>,
    controls: VecDeque<Control>,
    seq: u64,
    quit: bool,
    time_source: Option<TimeSource>,
}

/// The roster's handle on a running node.
pub(crate) struct NodeHandle {
    pub(crate) id: NodeId,
    pub(crate) name: String,
    pub(crate) kinds: Kinds,
    state: Mutex<LoopState>,
    wake: Condvar,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl NodeHandle {
    pub(crate) fn spawn(
        id: NodeId,
        node: Box<dyn MediaNode>,
        roster: Weak<RosterInner>,
        ts: TimeSource,
    ) -> Result<Arc<NodeHandle>> {
        let handle = Arc::new(NodeHandle {
            id,
            name: node.name(),
            kinds: node.kinds(),
            state: Mutex::new(LoopState::default()),
            wake: Condvar::new(),
            thread: Mutex::new(None),
        });
        let h = Arc::clone(&handle);
        let ctx = NodeContext {
            id,
            roster,
            time_source: ts,
            run_state: RunState::Stopped,
        };
        let t = std::thread::Builder::new()
            .name(format!("mkff node {id}: {}", handle.name))
            .spawn(move || h.run(node, ctx))
            .map_err(|e| Error::Os(format!("spawn node thread: {e}")))?;
        *handle.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
        Ok(handle)
    }

    pub(crate) fn post(&self, time: i64, event: MediaEvent) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.seq += 1;
        let seq = s.seq;
        s.queue.push(Entry { time, seq, event });
        self.wake.notify_one();
    }

    pub(crate) fn control(&self, c: Control) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.controls.push_back(c);
        self.wake.notify_one();
    }

    /// Run `f` on the node's thread and wait for its result.
    pub(crate) fn call<R: Send + 'static>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&mut dyn MediaNode, &mut NodeContext) -> R + Send + 'static,
    ) -> Result<R> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.control(Box::new(move |n, c| {
            let _ = tx.send(f(n, c));
        }));
        rx.recv_timeout(timeout).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => Error::TimedOut,
            mpsc::RecvTimeoutError::Disconnected => {
                Error::BadNode(format!("node {} exited", self.id))
            }
        })
    }

    pub(crate) fn set_time_source(&self, ts: TimeSource) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.time_source = Some(ts);
        self.wake.notify_one();
    }

    pub(crate) fn quit(&self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.quit = true;
            self.wake.notify_one();
        }
        let t = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(t) = t {
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }

    fn run(&self, mut node: Box<dyn MediaNode>, mut ctx: NodeContext) {
        node.node_registered(&mut ctx);
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if guard.quit {
                break;
            }
            if let Some(ts) = guard.time_source.take() {
                ctx.time_source = ts;
            }
            if let Some(c) = guard.controls.pop_front() {
                drop(guard);
                c(node.as_mut(), &mut ctx);
                guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                continue;
            }
            let Some(top) = guard.queue.peek() else {
                guard = self.wake.wait(guard).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            let now = Instant::now();
            let due = if top.time == IMMEDIATE {
                now
            } else {
                ctx.time_source.real_time_for(top.time, node.latency())
            };
            if due <= now {
                let Some(e) = guard.queue.pop() else { continue };
                drop(guard);
                let lateness = if e.time == IMMEDIATE {
                    0
                } else {
                    ctx.time_source.now() - e.time
                };
                match &e.event {
                    MediaEvent::Start => ctx.run_state = RunState::Started,
                    MediaEvent::Stop => ctx.run_state = RunState::Stopped,
                    _ => {}
                }
                node.handle_event(e.event, e.time, lateness, &mut ctx);
                guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            } else {
                // a stopped time source answers "an hour from now": re-check periodically so a
                // later start / publish is noticed
                let wait = (due - now).min(Duration::from_millis(50));
                guard = self
                    .wake
                    .wait_timeout(guard, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
    }
}
