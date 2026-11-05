//! The session: one worker thread, one explicit state machine, one reconnect
//! policy.
//!
//! ```text
//! Disconnected ─▶ Connecting ─▶ Authenticating ─▶ Joining ─▶ Connected
//!       ▲                                                        │
//!       │                                                        ▼
//!    Closed ◀── Leaving ◀────────────────────────────────── Reconnecting
//!       ▲                                                        │
//!       └──────────────────── Failed ◀───────────────────────────┘
//! ```
//!
//! One enum rather than a scattering of `is_connected` / `is_joining` /
//! `is_reconnecting` booleans, because those three can be true at once and the
//! resulting state is not one anybody designed. Every transition here is a
//! single assignment and every assignment publishes an event.
//!
//! The worker owns the signaling socket, the registry and the media runtime.
//! Callers on other threads speak to it through a command channel and read a
//! snapshot; nothing outside this module touches the socket.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::bridge::{JamAudioSink, JamPublishRequest, JamPublishSource};
use crate::clock::{self, SessionClock};
use crate::config::JamConfig;
use crate::credentials::SharedCredentials;
use crate::error::{JamError, Result};
use crate::ids::{JamId, ParticipantId, StreamId};
use crate::media::{MediaRuntime, MediaShared, Publication, StreamRuntimeStats, SubscribedStream};
use crate::protocol::{
    self, message, AudioCapabilities, AudioCodec, AudioFormat, AudioFormatSelected,
    ChannelMetadata, ClockSyncRequest, ClockSyncResponse, CodecCapability, JamClosed,
    JamJoinRequest, JamJoined, JamLeaveRequest, JamParticipantEvent, JamParticipantState,
    ParticipantSummary, RegionSummary, SampleFormat, StreamEvent, StreamPublishRequest,
    StreamPublished, StreamSubscribeRequest, StreamSubscribed, StreamSummary,
    StreamUnpublishRequest, StreamUnsubscribeRequest, StreamUnsubscribed, SubscriptionMode,
    SubscriptionRefusal, TransportCandidates, TransportCapabilities, TransportKind,
    TransportSelect, TransportSelected, UserSummary, MAX_STREAM_CHANNELS,
};
use crate::registry::JamRegistry;
use crate::signaling::SignalingClient;
use crate::transport::{self, TransportStatsSnapshot};

/// Where the client is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JamState {
    Disconnected,
    Connecting,
    Authenticating,
    Joining,
    Connected,
    Reconnecting,
    Leaving,
    Closed,
    Failed,
}

impl JamState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting",
            Self::Authenticating => "Authenticating",
            Self::Joining => "Joining",
            Self::Connected => "Connected",
            Self::Reconnecting => "Reconnecting",
            Self::Leaving => "Leaving",
            Self::Closed => "Closed",
            Self::Failed => "Failed",
        }
    }

    /// Whether audio can be flowing in this state.
    pub fn live(self) -> bool {
        matches!(self, Self::Connected)
    }

    /// Whether the session is over for good.
    pub fn terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Failed)
    }
}

/// What a caller asks the worker to do.
pub enum JamCommand {
    /// Enter a jam. `access_token` comes from an invite exchange and may be
    /// empty when the caller is the host or already a member.
    Join {
        jam_id: JamId,
        access_token: String,
    },
    /// Give up the participant slot.
    Leave,
    /// Publish a stream fed by `source`.
    Publish {
        request: JamPublishRequest,
        source: Arc<dyn JamPublishSource>,
    },
    Unpublish(StreamId),
    /// Start receiving these streams as well.
    Subscribe(Vec<StreamId>),
    /// Stop receiving these streams.
    Unsubscribe(Vec<StreamId>),
    /// Take the whole room again, letting the server choose.
    SubscribeEverything,
    /// Receive nothing until something is asked for.
    SubscribeNothing,
    /// Stop the worker entirely.
    Shutdown,
}

/// What the worker reports.
#[derive(Debug, Clone)]
pub enum JamEvent {
    State(JamState),
    /// The account this connection acts as, as the server sees it.
    Identified(UserSummary),
    Joined {
        participant: ParticipantSummary,
        region: RegionSummary,
        resumed: bool,
    },
    ParticipantJoined(ParticipantSummary),
    ParticipantLeft {
        participant: ParticipantSummary,
        reason: String,
    },
    StreamAdded(StreamSummary),
    StreamRemoved(StreamId),
    /// The server resolved a format for this receiver: audio is on its way.
    FormatSelected {
        stream: StreamId,
        format: AudioFormat,
    },
    /// The set of streams this client receives changed, and what the server
    /// would not grant.
    ///
    /// `streams` is the whole set, not a delta, so a host can reconcile its
    /// routing against one value. It is empty under [`SubscriptionMode::Auto`],
    /// where the server decides and a snapshot would go stale on the next
    /// publish.
    IngressChanged {
        mode: SubscriptionMode,
        streams: Vec<StreamId>,
        refused: Vec<SubscriptionRefusal>,
    },
    Published(StreamSummary),
    Unpublished(StreamId),
    TransportSelected {
        kind: TransportKind,
        node_id: String,
        rtt_ms: f64,
    },
    /// The jam ended for everybody.
    Closed(String),
    /// Something went wrong. `fatal` says whether the worker gave up.
    Error {
        message: String,
        detail: String,
        fatal: bool,
    },
}

/// A cheap, cloneable view of the room for a UI frame.
#[derive(Debug, Clone, Default)]
pub struct JamSnapshot {
    pub state_label: &'static str,
    pub jam_id: Option<JamId>,
    pub jam_name: String,
    pub public_id: String,
    pub join_url: String,
    pub region: RegionSummary,
    pub self_participant: Option<ParticipantSummary>,
    pub account: Option<UserSummary>,
    pub participants: Vec<ParticipantSummary>,
    pub streams: Vec<StreamSummary>,
    /// Streams the server has resolved a format for, i.e. the ones actually
    /// arriving.
    pub formats: Vec<(StreamId, AudioFormat)>,
    pub transport: Option<TransportKind>,
    pub transport_stats: TransportStatsSnapshot,
    pub stream_stats: Vec<(StreamId, StreamRuntimeStats)>,
    pub rtt_ms: f64,
    pub clock_offset_ms: f64,
    pub clock_drift_ppm: f64,
    pub clock_locked: bool,
    pub last_error: Option<String>,
}

/// Everything the worker needs that is not configuration.
pub struct JamSessionOptions {
    /// Stable across restarts, so a reconnecting Studio is recognised as the
    /// same device rather than accumulating ghost participants.
    pub device_id: String,
    pub device_name: String,
    /// Where decoded remote audio goes.
    pub sink: Arc<dyn JamAudioSink>,
    /// The rates and formats this host can actually handle. Sent verbatim as
    /// `audio.capabilities`; the server picks from it per stream.
    pub capabilities: AudioCapabilities,
    /// What this client offers to open for media.
    ///
    /// Defaults to everything this build implements. A user who already knows
    /// their network permits nothing but outbound HTTPS can narrow it to
    /// [`transport::reliable_only_capabilities`] and skip a failed connection
    /// attempt per datagram candidate on every join.
    pub transport: TransportCapabilities,
    /// Frame length to ask for when publishing, in samples per channel.
    pub publish_frame_samples: i32,
    /// Sample rate to publish at. Independent of the project rate: the jam
    /// branch resamples, the project does not change.
    pub publish_sample_rate: i32,
    /// Which streams this client wants the server to send it.
    pub ingress: JamIngress,
}

/// The ingress policy this client joins with.
///
/// Egress was always explicit — a client says what it publishes. Ingress was
/// not: the server sent every participant every stream it could decode. That is
/// right for a listener and wrong for a DAW, where one performer routed to one
/// track should cost one stream rather than the whole room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JamIngress {
    /// Take the room. The server chooses, exactly as it always did, and this is
    /// the default so no existing caller changes behaviour.
    #[default]
    Everything,
    /// Take only what the host asks for. The session goes silent the moment it
    /// joins and stays silent until [`JamSession::subscribe`] names something,
    /// which is what keeps a project with two routed tracks from paying for a
    /// six-piece band.
    Routed,
}

impl JamSessionOptions {
    /// Capabilities for a Studio-class host: PCM at every rate the server
    /// negotiates, in all three sample formats, mono or stereo.
    ///
    /// AAC-LC is deliberately absent. The protocol carries it and the server
    /// will negotiate it, but this build has no encoder or decoder, and
    /// offering a codec that cannot be decoded would subscribe this client to
    /// silence.
    pub fn studio_capabilities() -> AudioCapabilities {
        AudioCapabilities {
            codecs: vec![CodecCapability {
                codec: AudioCodec::Pcm,
                sample_rates: vec![44100, 48000, 88200, 96000, 176400, 192000],
                bitrates: Vec::new(),
                formats: vec![
                    SampleFormat::F32Le,
                    SampleFormat::S24Le,
                    SampleFormat::S16Le,
                ],
                // Every layout up to the protocol ceiling, because this client
                // both publishes and receives multitrack takes. Listing them is
                // what lets a receiving Studio be offered a wide stream at all;
                // a browser listener that offers only 1 and 2 is refused one by
                // the server rather than handed channels it cannot decode.
                channels: (1..=MAX_STREAM_CHANNELS as i32).collect(),
                // Sized for the stereo streams that are almost all of them: the
                // server picks the smallest size both sides offer, so listing a
                // very short one here would drag every ordinary stream up to a
                // needless packet rate. A wide stream states its own shorter
                // size per publish instead — see
                // [`crate::bridge::JamPublishRequest::frame_sizes`].
                frame_sizes: vec![128, 256, 512, 1024],
            }],
        }
    }

    pub fn new(device_id: impl Into<String>, sink: Arc<dyn JamAudioSink>) -> Self {
        Self {
            device_id: device_id.into(),
            device_name: "Futureboard Studio".to_string(),
            sink,
            capabilities: Self::studio_capabilities(),
            transport: transport::native_capabilities(),
            // Overridden by the smallest advertised frame size at publish time;
            // this is only the fallback for a caller that supplies capabilities
            // with no frame sizes at all.
            publish_frame_samples: 128,
            publish_sample_rate: 48_000,
            ingress: JamIngress::Everything,
        }
    }
}

/// Handle to a running jam worker.
pub struct JamSession {
    commands: Sender<JamCommand>,
    events: Receiver<JamEvent>,
    snapshot: Arc<RwLock<JamSnapshot>>,
    state: Arc<RwLock<JamState>>,
    clock: Arc<Mutex<SessionClock>>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl JamSession {
    /// Start the worker. It idles in `Disconnected` until told to join.
    pub fn spawn(
        config: JamConfig,
        credentials: SharedCredentials,
        options: JamSessionOptions,
    ) -> Result<Self> {
        let clock = Arc::new(Mutex::new(SessionClock::default()));
        Self::spawn_with_clock(config, credentials, options, clock)
    }

    /// Start the worker against a clock the caller already holds.
    ///
    /// Studio needs this: its publish tap stamps capture timestamps and its
    /// recorder places remote takes, and both have to read the same clock the
    /// worker is syncing rather than a copy of it. A copy would be a few
    /// milliseconds stale, which is exactly the error the session clock exists
    /// to remove.
    pub fn spawn_with_clock(
        config: JamConfig,
        credentials: SharedCredentials,
        options: JamSessionOptions,
        clock: Arc<Mutex<SessionClock>>,
    ) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let snapshot = Arc::new(RwLock::new(JamSnapshot {
            state_label: JamState::Disconnected.label(),
            ..Default::default()
        }));
        let state = Arc::new(RwLock::new(JamState::Disconnected));
        let running = Arc::new(AtomicBool::new(true));

        let worker = {
            let snapshot = Arc::clone(&snapshot);
            let state = Arc::clone(&state);
            let clock = Arc::clone(&clock);
            let running = Arc::clone(&running);
            std::thread::Builder::new()
                .name("jam-control".to_string())
                .spawn(move || {
                    let mut worker = Worker::new(
                        config,
                        credentials,
                        options,
                        command_rx,
                        event_tx,
                        snapshot,
                        state,
                        clock,
                    );
                    worker.run();
                    running.store(false, Ordering::Release);
                })
                .map_err(|error| {
                    JamError::Session(format!("could not start the jam worker: {error}"))
                })?
        };

        Ok(Self {
            commands: command_tx,
            events: event_rx,
            snapshot,
            state,
            clock,
            running,
            worker: Some(worker),
        })
    }

    pub fn state(&self) -> JamState {
        self.state
            .read()
            .map(|guard| *guard)
            .unwrap_or(JamState::Failed)
    }

    pub fn snapshot(&self) -> JamSnapshot {
        self.snapshot
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The session clock, shared with the worker that keeps it synced.
    ///
    /// A publish source needs it to stamp a capture timestamp, and a recorder
    /// needs it to place a remote take on the project timeline. It is behind a
    /// mutex rather than copied into a snapshot because both of those are exact
    /// conversions, not display values, and a stale anchor would move a
    /// waveform. Never locked from an audio callback.
    pub fn clock(&self) -> Arc<Mutex<SessionClock>> {
        Arc::clone(&self.clock)
    }

    /// Drain whatever the worker has reported since the last call. Never
    /// blocks, so a UI frame can call it unconditionally.
    pub fn drain_events(&self) -> Vec<JamEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }

    pub fn join(&self, jam_id: JamId, access_token: impl Into<String>) -> Result<()> {
        self.send(JamCommand::Join {
            jam_id,
            access_token: access_token.into(),
        })
    }

    pub fn leave(&self) -> Result<()> {
        self.send(JamCommand::Leave)
    }

    pub fn publish(
        &self,
        request: JamPublishRequest,
        source: Arc<dyn JamPublishSource>,
    ) -> Result<()> {
        self.send(JamCommand::Publish { request, source })
    }

    pub fn unpublish(&self, stream: StreamId) -> Result<()> {
        self.send(JamCommand::Unpublish(stream))
    }

    /// Start receiving these streams, in addition to whatever is already
    /// arriving.
    ///
    /// A stream that is not published yet is remembered rather than refused: a
    /// track bound to a performer who has not started is a track waiting, and
    /// it attaches by itself when that performer publishes.
    pub fn subscribe(&self, streams: Vec<StreamId>) -> Result<()> {
        self.send(JamCommand::Subscribe(streams))
    }

    /// Stop receiving these streams. The bandwidth stops with them.
    pub fn unsubscribe(&self, streams: Vec<StreamId>) -> Result<()> {
        self.send(JamCommand::Unsubscribe(streams))
    }

    /// Take the whole room, letting the server choose again. This is the
    /// listener's view.
    pub fn subscribe_everything(&self) -> Result<()> {
        self.send(JamCommand::SubscribeEverything)
    }

    /// Receive nothing until something is asked for.
    pub fn subscribe_nothing(&self) -> Result<()> {
        self.send(JamCommand::SubscribeNothing)
    }

    fn send(&self, command: JamCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| JamError::Session("the jam worker has stopped".to_string()))
    }
}

impl Drop for JamSession {
    fn drop(&mut self) {
        let _ = self.commands.send(JamCommand::Shutdown);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

// ── worker ──────────────────────────────────────────────────────────────────

/// Consecutive failed clock syncs that mark the signaling link dead.
const MISSED_SYNCS_BEFORE_DROP: u32 = 2;

/// Reconnect delays. Doubling from a quarter second, capped by configuration
/// and jittered so a server restart does not bring every client back at once.
const BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Pending publish requests survive a reconnect, so a stream that was live
/// before the drop is republished after it.
struct PendingPublish {
    request: JamPublishRequest,
    source: Arc<dyn JamPublishSource>,
}

struct Worker {
    config: JamConfig,
    credentials: SharedCredentials,
    options: JamSessionOptions,
    commands: Receiver<JamCommand>,
    events: Sender<JamEvent>,
    snapshot: Arc<RwLock<JamSnapshot>>,
    state: Arc<RwLock<JamState>>,

    registry: JamRegistry,
    clock: Arc<Mutex<SessionClock>>,
    media_shared: Arc<MediaShared>,
    media: Option<MediaRuntime>,

    jam_id: Option<JamId>,
    access_token: String,
    resume_token: String,
    participant_id: Option<ParticipantId>,
    /// Streams this client publishes, kept so a reconnect can restore them.
    publications: Vec<PendingPublish>,
    /// Stream ids currently published, in the same order as `publications`.
    published_ids: Vec<Option<StreamId>>,

    /// The ingress policy in force. It starts at the option the caller supplied
    /// and moves whenever a subscribe command arrives, so a reconnect restores
    /// what the host last asked for rather than what it started with.
    ingress: JamIngress,
    /// Streams the host asked for, whether or not they exist yet.
    ///
    /// Kept whole rather than pruned to what is published: a track bound to a
    /// performer who has not joined is not a mistake, and pruning would make
    /// that track re-bind by hand when they arrive.
    wanted: BTreeSet<StreamId>,
    /// What the server is believed to hold for this participant. The diff
    /// against `wanted` is what actually goes on the wire, so a routing change
    /// costs one message about the streams that moved rather than a restatement
    /// of the whole set.
    subscribed: BTreeSet<StreamId>,
    /// The ingress mode the server is believed to hold for this participant.
    /// A fresh join resets it to `Auto`, which is the server's own default; a
    /// resume leaves it, because a resumed participant keeps both its declared
    /// set and the routing-table entries behind it.
    server_mode: SubscriptionMode,
    /// Set when the wanted set and the room have diverged — a stream arrived
    /// that a track is waiting for, or a routing command came in — so the pump
    /// reconciles once, outside the event handler that noticed.
    ingress_dirty: bool,
    shutdown: bool,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: JamConfig,
        credentials: SharedCredentials,
        options: JamSessionOptions,
        commands: Receiver<JamCommand>,
        events: Sender<JamEvent>,
        snapshot: Arc<RwLock<JamSnapshot>>,
        state: Arc<RwLock<JamState>>,
        clock: Arc<Mutex<SessionClock>>,
    ) -> Self {
        let options_ingress = options.ingress;
        Self {
            config,
            credentials,
            options,
            commands,
            events,
            snapshot,
            state,
            registry: JamRegistry::new(),
            clock,
            media_shared: Arc::new(MediaShared::new()),
            media: None,
            jam_id: None,
            access_token: String::new(),
            resume_token: String::new(),
            participant_id: None,
            publications: Vec::new(),
            published_ids: Vec::new(),
            ingress: options_ingress,
            wanted: BTreeSet::new(),
            subscribed: BTreeSet::new(),
            server_mode: SubscriptionMode::Auto,
            ingress_dirty: false,
            shutdown: false,
        }
    }

    fn run(&mut self) {
        while !self.shutdown {
            // Idle: nothing to do until somebody asks for a jam.
            match self.commands.recv_timeout(Duration::from_millis(200)) {
                Ok(command) => self.handle_idle_command(command),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.jam_id.is_none() || self.shutdown {
                continue;
            }
            self.connect_loop();
        }
        self.teardown_media();
        self.set_state(JamState::Closed);
    }

    fn handle_idle_command(&mut self, command: JamCommand) {
        match command {
            JamCommand::Join {
                jam_id,
                access_token,
            } => {
                self.jam_id = Some(jam_id);
                self.access_token = access_token;
                self.resume_token.clear();
                self.registry.clear();
            }
            JamCommand::Publish { request, source } => {
                // Queued: it is published as soon as a session exists, which is
                // what lets a caller arm a publish before joining.
                self.publications.push(PendingPublish { request, source });
                self.published_ids.push(None);
            }
            JamCommand::Unpublish(stream) => self.drop_publication(&stream),
            // Routing before there is a socket is normal: a project opens with
            // its tracks already bound to performers. The intent is recorded
            // and applied the moment a session exists.
            JamCommand::Subscribe(streams) => self.want_streams(streams),
            JamCommand::Unsubscribe(streams) => self.unwant_streams(&streams),
            JamCommand::SubscribeEverything => self.want_everything(),
            JamCommand::SubscribeNothing => self.want_nothing(),
            JamCommand::Leave => {
                self.jam_id = None;
                self.set_state(JamState::Disconnected);
            }
            JamCommand::Shutdown => self.shutdown = true,
        }
    }

    /// Add streams to what this client is asking for.
    fn want_streams(&mut self, streams: Vec<StreamId>) {
        // Asking for a specific stream is taking control. Leaving the policy at
        // `Everything` would mean the server kept sending the rest of the room
        // alongside, which is the cost this exists to avoid.
        self.ingress = JamIngress::Routed;
        for stream in streams {
            self.wanted.insert(stream);
        }
        self.ingress_dirty = true;
    }

    fn unwant_streams(&mut self, streams: &[StreamId]) {
        self.ingress = JamIngress::Routed;
        for stream in streams {
            self.wanted.remove(stream);
        }
        self.ingress_dirty = true;
    }

    fn want_everything(&mut self) {
        self.ingress = JamIngress::Everything;
        self.wanted.clear();
        self.ingress_dirty = true;
    }

    fn want_nothing(&mut self) {
        self.ingress = JamIngress::Routed;
        self.wanted.clear();
        self.ingress_dirty = true;
    }

    /// Attempt, run, and if the drop was recoverable, attempt again.
    fn connect_loop(&mut self) {
        let mut attempt: u32 = 0;
        while !self.shutdown && self.jam_id.is_some() {
            let resuming = !self.resume_token.is_empty();
            self.set_state(if resuming {
                JamState::Reconnecting
            } else {
                JamState::Connecting
            });

            let outcome = self.run_session();
            // Whatever happened, the transport that was open belongs to the
            // attempt that just ended. Left running it would keep sending on a
            // connection generation the server has already superseded, and keep
            // a socket and two threads alive through the whole backoff.
            if !matches!(outcome, Ok(SessionEnd::Left) | Ok(SessionEnd::Shutdown)) {
                self.teardown_media();
            }

            match outcome {
                Ok(SessionEnd::Left) | Ok(SessionEnd::Shutdown) => return,
                Ok(SessionEnd::JamClosed(reason)) => {
                    self.emit(JamEvent::Closed(reason));
                    self.jam_id = None;
                    self.set_state(JamState::Closed);
                    return;
                }
                Ok(SessionEnd::Dropped) => {}
                Err(error) => {
                    let fatal = !error.recoverable() || !self.config.reconnect;
                    self.record_error(&error, fatal);
                    if fatal {
                        self.jam_id = None;
                        self.set_state(JamState::Failed);
                        return;
                    }
                    if matches!(
                        &error,
                        JamError::Api(wire) if wire.code == crate::error::ErrorCode::ResumeRejected
                    ) {
                        // The participant is gone. Join fresh rather than
                        // retrying a token the server has already refused.
                        self.resume_token.clear();
                    }
                }
            }

            if !self.config.reconnect {
                self.jam_id = None;
                self.set_state(JamState::Disconnected);
                return;
            }

            attempt = attempt.saturating_add(1);
            self.set_state(JamState::Reconnecting);
            if !self.sleep_backoff(attempt) {
                return;
            }
        }
    }

    /// Sleep the backoff for `attempt`, returning false if the worker was told
    /// to stop while waiting. Cancellable rather than a plain sleep, so leaving
    /// during a reconnect is immediate and no task is left running.
    fn sleep_backoff(&mut self, attempt: u32) -> bool {
        let doubled = BACKOFF_BASE
            .checked_mul(1u32 << attempt.min(6))
            .unwrap_or(self.config.reconnect_max_delay);
        let capped = doubled.min(self.config.reconnect_max_delay);
        // ±20 % of jitter, so a server restart does not bring every client back
        // in the same millisecond.
        let jitter = 0.8 + rand::random::<f64>() * 0.4;
        let delay = capped.mul_f64(jitter);

        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            match self.commands.recv_timeout(Duration::from_millis(100)) {
                Ok(JamCommand::Shutdown) => {
                    self.shutdown = true;
                    return false;
                }
                Ok(JamCommand::Leave) => {
                    self.jam_id = None;
                    self.set_state(JamState::Disconnected);
                    return false;
                }
                Ok(command) => self.queue_during_backoff(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.shutdown = true;
                    return false;
                }
            }
        }
        true
    }

    fn queue_during_backoff(&mut self, command: JamCommand) {
        match command {
            JamCommand::Publish { request, source } => {
                self.publications.push(PendingPublish { request, source });
                self.published_ids.push(None);
            }
            JamCommand::Unpublish(stream) => self.drop_publication(&stream),
            JamCommand::Join {
                jam_id,
                access_token,
            } => {
                if Some(&jam_id) != self.jam_id.as_ref() {
                    self.resume_token.clear();
                    self.registry.clear();
                }
                self.jam_id = Some(jam_id);
                self.access_token = access_token;
            }
            JamCommand::Subscribe(streams) => self.want_streams(streams),
            JamCommand::Unsubscribe(streams) => self.unwant_streams(&streams),
            JamCommand::SubscribeEverything => self.want_everything(),
            JamCommand::SubscribeNothing => self.want_nothing(),
            JamCommand::Leave | JamCommand::Shutdown => {}
        }
    }

    /// One full attempt: connect, join, negotiate, then pump until it ends.
    fn run_session(&mut self) -> Result<SessionEnd> {
        let Some(jam_id) = self.jam_id.clone() else {
            return Ok(SessionEnd::Left);
        };
        let token = self.account_token()?;

        self.set_state(JamState::Connecting);
        let (mut signaling, ready) = SignalingClient::connect(
            &self.config.websocket_url,
            &token,
            self.config.connect_timeout,
        )?;
        self.set_state(JamState::Authenticating);
        self.emit(JamEvent::Identified(ready.user.clone()));
        self.update_snapshot(|snapshot| snapshot.account = Some(ready.user.clone()));

        self.set_state(JamState::Joining);
        let joined = self.join_room(&mut signaling, &jam_id)?;
        if !joined.resumed {
            // A fresh participant starts at the server's own default. A resumed
            // one keeps its declared set and the routing entries behind it, so
            // its believed state is still accurate and must not be discarded.
            self.server_mode = SubscriptionMode::Auto;
            self.subscribed.clear();
        }

        // Silence first, and before capabilities rather than after: capabilities
        // are what make a receiver eligible, so a client that wants to choose
        // has to say so while it is still ineligible. Asking afterwards pays for
        // the whole room for one round trip, on the link that is the constraint.
        if self.ingress == JamIngress::Routed {
            self.silence_ingress(&mut signaling, &jam_id)?;
        }

        // Codec capabilities before anything else: the server resolves a format
        // per stream per receiver and cannot pick one for a client that has not
        // said what it can decode.
        let capabilities = self.options.capabilities.clone();
        let _ack: protocol::Ack = signaling.request(
            message::AUDIO_CAPABILITIES,
            &capabilities,
            message::AUDIO_CAPABILITIES,
            self.config.connect_timeout,
        )?;

        // Now that a format can be resolved, ask for what the host routed.
        self.ingress_dirty = true;
        if let Err(error) = self.reconcile_ingress(&mut signaling, &jam_id) {
            // A refused subscription is not a reason to fail the session: the
            // room is joined, the transport is about to open, and the host can
            // re-route. Failing here would turn one unroutable performer into a
            // dropped jam.
            if !error.recoverable() {
                return Err(error);
            }
            self.record_error(&error, false);
        }

        self.open_media(&mut signaling, joined.resumed)?;
        self.restore_publications(&mut signaling, &jam_id)?;

        self.set_state(JamState::Connected);
        self.pump(&mut signaling, &jam_id)
    }

    fn account_token(&self) -> Result<String> {
        if let Some(dev) = self.config.dev_token.as_ref() {
            // Development only; `JamConfig` refuses to carry one otherwise.
            return Ok(dev.clone());
        }
        self.credentials.access_token()
    }

    fn join_room(&mut self, signaling: &mut SignalingClient, jam_id: &JamId) -> Result<JamJoined> {
        let request = JamJoinRequest {
            jam_id: jam_id.as_str().to_string(),
            jam_access_token: self.access_token.clone(),
            device_id: self.options.device_id.clone(),
            device_name: self.options.device_name.clone(),
            resume_token: self.resume_token.clone(),
            last_seq: self.registry.seq(),
            preferred_region: self.config.preferred_region.wire_value().to_string(),
            region_probes: Vec::new(),
        };
        let joined: JamJoined = signaling.request(
            message::JAM_JOIN,
            &request,
            message::JAM_JOINED,
            self.config.connect_timeout,
        )?;

        self.resume_token = joined.resume_token.clone();
        self.participant_id = Some(ParticipantId::new(joined.participant.id.clone()));
        let ended = self.registry.reset(
            joined.participants.clone(),
            joined.streams.clone(),
            joined.seq,
        );
        // Streams that ended while this client was away; see `reset`.
        for stream in ended {
            self.options.sink.stream_ended(&stream);
            self.emit(JamEvent::StreamRemoved(stream));
        }
        if joined.clock.rate != 0 {
            if let Ok(mut clock) = self.clock.lock() {
                clock.set_rate(joined.clock.rate);
            }
        }

        let jam = joined.jam.clone();
        let region = joined.region.clone();
        let participant = joined.participant.clone();
        self.update_snapshot(move |snapshot| {
            snapshot.jam_id = Some(JamId::new(jam.id.clone()));
            snapshot.jam_name = jam.name.clone();
            snapshot.public_id = jam.public_id.clone();
            snapshot.region = region.clone();
            snapshot.self_participant = Some(participant.clone());
        });
        self.publish_registry_snapshot();
        self.emit(JamEvent::Joined {
            participant: joined.participant.clone(),
            region: joined.region.clone(),
            resumed: joined.resumed,
        });
        Ok(joined)
    }

    /// Negotiate a transport, open it, and report what actually opened.
    fn open_media(&mut self, signaling: &mut SignalingClient, resumed: bool) -> Result<()> {
        self.teardown_media();

        let candidates: TransportCandidates = signaling.request(
            message::TRANSPORT_CAPABILITIES,
            &self.options.transport,
            message::TRANSPORT_CANDIDATES,
            self.config.connect_timeout,
        )?;

        // Signaling is serviced between candidates so the server's heartbeat is
        // answered while fallbacks are tried; see `connect_first_with`.
        let opened = transport::connect_first_with(
            &candidates.candidates,
            self.config.connect_timeout,
            resumed,
            || signaling.service(),
        )?;
        let kind = opened.kind;
        let node_id = opened.welcome.node_id.clone();
        let candidate_id = opened.candidate_id.clone();
        let rtt = opened.handshake_rtt;

        // Echo the candidate's own token: it is what proves the selection
        // refers to a candidate this server offered.
        let token = candidates
            .candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
            .map(|candidate| candidate.token.clone())
            .unwrap_or_default();

        let runtime = MediaRuntime::start(
            opened,
            Arc::clone(&self.media_shared),
            Arc::clone(&self.options.sink),
        );
        self.media = Some(runtime);
        self.sync_subscriptions();

        let _selected: TransportSelected = signaling.request(
            message::TRANSPORT_SELECT,
            &TransportSelect {
                candidate_id: candidate_id.clone(),
                kind,
                token,
                rtt_ms: rtt.as_secs_f64() * 1000.0,
            },
            message::TRANSPORT_SELECTED,
            self.config.connect_timeout,
        )?;

        self.update_snapshot(|snapshot| snapshot.transport = Some(kind));
        self.emit(JamEvent::TransportSelected {
            kind,
            node_id,
            rtt_ms: rtt.as_secs_f64() * 1000.0,
        });
        Ok(())
    }

    /// Re-publish everything this client had live before the drop.
    fn restore_publications(
        &mut self,
        signaling: &mut SignalingClient,
        jam_id: &JamId,
    ) -> Result<()> {
        // The media plane's aliases are per session, so every publication has
        // to be minted again; the stream *names* are what receivers rebind by.
        if let Ok(mut guard) = self.media_shared.publications.lock() {
            guard.clear();
        }
        for index in 0..self.publications.len() {
            self.published_ids[index] = None;
            if let Err(error) = self.publish_one(signaling, jam_id, index) {
                // One stream failing to publish must not take the session down;
                // the others are still music.
                self.record_error(&error, false);
            }
        }
        Ok(())
    }

    /// A stream this participant already publishes under the same name.
    ///
    /// After a resumed join the participant keeps its streams — that is the
    /// point of resuming — so the snapshot already lists them. Publishing again
    /// would mint a second stream with a second alias and leave the room
    /// showing one guitar per reconnect, each one silent except the newest.
    fn adoptable_stream(&self, name: &str) -> Option<StreamSummary> {
        let mine = self.participant_id.as_ref()?;
        self.registry
            .streams()
            .into_iter()
            .find(|stream| &stream.participant_id == mine && stream.name == name)
            .map(|stream| stream.summary.clone())
    }

    fn publish_one(
        &mut self,
        signaling: &mut SignalingClient,
        jam_id: &JamId,
        index: usize,
    ) -> Result<()> {
        let Some(pending) = self.publications.get(index) else {
            return Ok(());
        };
        let channels = pending.request.channels.clamp(1, MAX_STREAM_CHANNELS) as i32;
        let labels = pending.request.channel_labels.clone();
        let sample_format = if pending.request.sample_format == SampleFormat::None {
            SampleFormat::F32Le
        } else {
            pending.request.sample_format
        };
        let sample_rate = if pending.request.sample_rate > 0 {
            pending.request.sample_rate
        } else {
            self.options.publish_sample_rate
        };
        let frame_sizes = pending.request.frame_sizes.clone();

        // Re-attach to the stream this participant already has, if it kept one
        // across the reconnect.
        if let Some(existing) = self.adoptable_stream(&pending.request.name) {
            self.attach_publication(index, &existing);
            self.emit(JamEvent::Published(existing));
            return Ok(());
        }
        let request = StreamPublishRequest {
            jam_id: jam_id.as_str().to_string(),
            name: pending.request.name.clone(),
            direction: protocol::direction::SEND.to_string(),
            codec: AudioCodec::Pcm,
            sample_rate,
            sample_format,
            channels,
            channel_metadata: labels
                .iter()
                .enumerate()
                .map(|(index, label)| ChannelMetadata {
                    index: index as i32,
                    label: label.clone(),
                    role: label.clone(),
                })
                .collect(),
            frame_sizes,
            clock_domain: protocol::DOMAIN_SESSION.to_string(),
            latency: pending.source.latency(),
        };

        let published: StreamPublished = signaling.request(
            message::JAM_STREAM_PUBLISH,
            &request,
            message::JAM_STREAM_PUBLISHED,
            self.config.connect_timeout,
        )?;

        self.attach_publication(index, &published.stream);
        self.emit(JamEvent::Published(published.stream));
        Ok(())
    }

    /// Point a publish source at a stream and start feeding it.
    ///
    /// Shared by a fresh publish and a re-attach, so both paths install exactly
    /// the same media-plane state: the same alias, the same format, and a
    /// sequence counter that starts again. Restarting the sequence is correct
    /// because a re-attach comes with a new connection generation, and a
    /// receiver's jitter buffer discards everything from the old one.
    fn attach_publication(&mut self, index: usize, stream: &StreamSummary) {
        let Some(source) = self
            .publications
            .get(index)
            .map(|pending| Arc::clone(&pending.source))
        else {
            return;
        };
        // The format the packets actually carry is the one the *server*
        // acknowledged, not the one this client asked for. They agree today,
        // but reading it back from the summary is what keeps a header honest if
        // the server ever normalises a field.
        let requested = self
            .publications
            .get(index)
            .map(|pending| &pending.request)
            .map(|request| (request.sample_format, request.frame_sizes.clone()))
            .unwrap_or((SampleFormat::F32Le, Vec::new()));
        let stream_id = StreamId::new(stream.id.clone());
        let sample_format = if stream.sample_format == SampleFormat::None {
            requested.0
        } else {
            stream.sample_format
        };
        let format = AudioFormat {
            codec: AudioCodec::Pcm,
            sample_rate: if stream.sample_rate > 0 {
                stream.sample_rate
            } else {
                self.options.publish_sample_rate
            },
            channels: stream.channels.clamp(1, MAX_STREAM_CHANNELS as i32),
            format: sample_format,
            bitrate: 0,
            frame_samples: self.publish_frame_samples(&requested.1),
        };
        if let Ok(mut guard) = self.media_shared.publications.lock() {
            guard.retain(|publication| publication.stream_id != stream_id);
            guard.push(Publication::new(
                stream_id.clone(),
                stream.media_alias,
                format,
                source,
            ));
        }
        self.published_ids[index] = Some(stream_id);

        // The room broadcast for a new stream excludes its own publisher, so
        // without this a client would never see what it is itself publishing.
        // The panel needs it, and so does anything that asks "am I live".
        self.registry.upsert_stream(stream.clone());
        self.publish_registry_snapshot();
    }

    /// The frame length to actually send, in samples per channel.
    ///
    /// It must be a size this client advertised in `audio.capabilities`, and
    /// specifically the *smallest* one: that is what the server's negotiator
    /// picks, and it is what every receiver is then told to expect. Sending a
    /// different length would work — the header carries the real frame count —
    /// but it would make `audio.format_selected` a statement nobody honoured,
    /// and the first thing anybody debugging a jitter problem would check.
    fn publish_frame_samples(&self, stream_frame_sizes: &[i32]) -> i32 {
        // A stream that stated its own sizes overrode the capability list for
        // negotiation, so it has to override it here too — otherwise the client
        // would send the session's frame length for a stream the server told
        // every receiver was much shorter.
        let offered = if stream_frame_sizes.is_empty() {
            self.options
                .capabilities
                .codecs
                .iter()
                .find(|capability| capability.codec == AudioCodec::Pcm)
                .map(|capability| capability.frame_sizes.as_slice())
                .unwrap_or_default()
        } else {
            stream_frame_sizes
        };
        offered
            .iter()
            .copied()
            .filter(|size| *size > 0)
            .min()
            .unwrap_or_else(|| self.options.publish_frame_samples.max(32))
    }

    fn drop_publication(&mut self, stream: &StreamId) {
        if let Some(index) = self
            .published_ids
            .iter()
            .position(|id| id.as_ref() == Some(stream))
        {
            self.publications.remove(index);
            self.published_ids.remove(index);
        }
        if let Ok(mut guard) = self.media_shared.publications.lock() {
            guard.retain(|publication| &publication.stream_id != stream);
        }
    }

    /// The connected loop: apply events, run commands, keep the clock synced.
    fn pump(&mut self, signaling: &mut SignalingClient, jam_id: &JamId) -> Result<SessionEnd> {
        let mut next_sync = Instant::now();
        let mut next_snapshot = Instant::now();
        let mut missed_syncs = 0u32;

        loop {
            if self.shutdown {
                self.leave_quietly(signaling, jam_id);
                return Ok(SessionEnd::Shutdown);
            }
            // A media thread noticing a dead socket is faster than waiting for
            // the signaling heartbeat to time out.
            if self.media.as_ref().is_some_and(|media| media.lost()) {
                return Ok(SessionEnd::Dropped);
            }

            match self.commands.try_recv() {
                Ok(JamCommand::Leave) => {
                    self.set_state(JamState::Leaving);
                    self.leave_quietly(signaling, jam_id);
                    self.jam_id = None;
                    self.resume_token.clear();
                    self.teardown_media();
                    self.set_state(JamState::Disconnected);
                    return Ok(SessionEnd::Left);
                }
                Ok(JamCommand::Shutdown) => {
                    self.shutdown = true;
                    continue;
                }
                Ok(JamCommand::Publish { request, source }) => {
                    self.publications.push(PendingPublish { request, source });
                    self.published_ids.push(None);
                    let index = self.publications.len() - 1;
                    if let Err(error) = self.publish_one(signaling, jam_id, index) {
                        self.record_error(&error, false);
                    }
                }
                Ok(JamCommand::Unpublish(stream)) => {
                    let request = StreamUnpublishRequest {
                        jam_id: jam_id.as_str().to_string(),
                        stream_id: stream.as_str().to_string(),
                    };
                    let _ = signaling.request::<protocol::StreamUnpublished, _>(
                        message::JAM_STREAM_UNPUBLISH,
                        &request,
                        message::JAM_STREAM_UNPUBLISHED,
                        self.config.connect_timeout,
                    );
                    self.registry.remove_stream(&stream);
                    self.drop_publication(&stream);
                    self.publish_registry_snapshot();
                    self.emit(JamEvent::Unpublished(stream));
                }
                Ok(JamCommand::Subscribe(streams)) => self.want_streams(streams),
                Ok(JamCommand::Unsubscribe(streams)) => self.unwant_streams(&streams),
                Ok(JamCommand::SubscribeEverything) => self.want_everything(),
                Ok(JamCommand::SubscribeNothing) => self.want_nothing(),
                Ok(JamCommand::Join { .. }) => {
                    // Already in a jam. One jam per session; a second one is a
                    // second session, which keeps event ordering unambiguous.
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.shutdown = true;
                    continue;
                }
            }

            // Reconciled here rather than where the divergence was noticed: a
            // stream event arrives inside the event handler, which has no socket
            // to answer on, and a routing command may arrive in the same turn as
            // three others. One reconcile per turn coalesces them into the
            // smallest set of messages that describes the change.
            if self.ingress_dirty {
                if let Err(error) = self.reconcile_ingress(signaling, jam_id) {
                    if !error.recoverable() {
                        return Err(error);
                    }
                    self.record_error(&error, false);
                }
            }

            if Instant::now() >= next_sync {
                next_sync = Instant::now() + clock::SYNC_INTERVAL;
                match self.sync_clock(signaling) {
                    Ok(()) => missed_syncs = 0,
                    Err(error) => {
                        if !error.recoverable() {
                            return Err(error);
                        }
                        // One missed sync is not worth ending a session over;
                        // the next one will land. Missing them in a row is
                        // what a half-open socket looks like — nothing comes
                        // back, nothing errors — and the session would read
                        // Connected until the OS gave up on the connection.
                        // Reconnecting resumes the same participant, so it
                        // costs far less than waiting.
                        missed_syncs += 1;
                        if missed_syncs >= MISSED_SYNCS_BEFORE_DROP {
                            return Ok(SessionEnd::Dropped);
                        }
                    }
                }
            }

            if let Some(envelope) = signaling.poll()? {
                if let Some(end) = self.apply_event(envelope)? {
                    return Ok(end);
                }
            }

            // Stats reach the UI at a bounded rate. Republishing the whole
            // snapshot per packet would make a busy jam a rerender storm.
            if Instant::now() >= next_snapshot {
                next_snapshot = Instant::now() + Duration::from_millis(33);
                self.publish_runtime_snapshot();
            }
        }
    }

    fn apply_event(&mut self, envelope: protocol::Envelope) -> Result<Option<SessionEnd>> {
        match envelope.kind.as_str() {
            message::JAM_PARTICIPANT_JOINED => {
                let event: JamParticipantEvent = envelope.decode()?;
                self.registry.observe_seq(event.seq);
                self.registry.upsert_participant(event.participant.clone());
                self.publish_registry_snapshot();
                self.emit(JamEvent::ParticipantJoined(event.participant));
            }
            message::JAM_PARTICIPANT_LEFT => {
                let event: JamParticipantEvent = envelope.decode()?;
                self.registry.observe_seq(event.seq);
                let gone = self
                    .registry
                    .remove_participant(&ParticipantId::new(event.participant.id.clone()));
                for stream in &gone {
                    self.options.sink.stream_ended(&stream.id);
                    self.emit(JamEvent::StreamRemoved(stream.id.clone()));
                }
                self.sync_subscriptions();
                self.publish_registry_snapshot();
                self.emit(JamEvent::ParticipantLeft {
                    participant: event.participant,
                    reason: event.reason,
                });
            }
            message::JAM_PARTICIPANT_STATE => {
                let event: JamParticipantState = envelope.decode()?;
                self.registry.observe_seq(event.seq);
                self.registry.set_participant_state(
                    &ParticipantId::new(event.participant_id),
                    &event.connection_state,
                    &event.transport,
                );
                self.publish_registry_snapshot();
            }
            message::JAM_STREAM_ADDED | message::JAM_STREAM_PUBLISHED => {
                let event: StreamEvent = envelope.decode()?;
                self.registry.observe_seq(event.seq);
                self.registry.upsert_stream(event.stream.clone());
                // A track bound to a performer who had not started yet is
                // waiting for exactly this. Reconciling attaches it without the
                // host having to notice the arrival and re-route by hand.
                if self
                    .wanted
                    .contains(&StreamId::new(event.stream.id.clone()))
                {
                    self.ingress_dirty = true;
                }
                self.sync_subscriptions();
                self.publish_registry_snapshot();
                self.emit(JamEvent::StreamAdded(event.stream));
            }
            message::JAM_STREAM_REMOVED => {
                let event: StreamEvent = envelope.decode()?;
                self.registry.observe_seq(event.seq);
                let id = if event.stream_id.is_empty() {
                    StreamId::new(event.stream.id.clone())
                } else {
                    StreamId::new(event.stream_id.clone())
                };
                self.registry.remove_stream(&id);
                self.options.sink.stream_ended(&id);
                // The server drops an unpublished stream from every declared
                // set, so forgetting it here keeps the two views in step. It
                // stays in `wanted`: the performer may publish again, and a
                // track that was pointed at them should reattach.
                self.subscribed.remove(&id);
                self.sync_subscriptions();
                self.publish_registry_snapshot();
                self.emit(JamEvent::StreamRemoved(id));
            }
            message::AUDIO_FORMAT_SELECTED => {
                let selection: AudioFormatSelected = envelope.decode()?;
                let id = StreamId::new(selection.stream_id.clone());
                if self.registry.set_stream_format(&id, selection.format) {
                    self.sync_subscriptions();
                    self.publish_registry_snapshot();
                    self.emit(JamEvent::FormatSelected {
                        stream: id,
                        format: selection.format,
                    });
                }
            }
            message::JAM_CLOSED => {
                let closed: JamClosed = envelope.decode()?;
                self.teardown_media();
                return Ok(Some(SessionEnd::JamClosed(closed.reason)));
            }
            message::ERROR => {
                return Err(JamError::Api(protocol::decode_error(&envelope)));
            }
            // A message type this build does not know is data, not a failure.
            // The server is allowed to grow.
            _ => {}
        }
        Ok(None)
    }

    /// Publish one clock exchange and fold the result in.
    fn sync_clock(&mut self, signaling: &mut SignalingClient) -> Result<()> {
        let t1 = clock::client_nanos();
        let response: ClockSyncResponse = signaling.request(
            message::CLOCK_SYNC_REQUEST,
            &ClockSyncRequest { t1, seq: 0 },
            message::CLOCK_SYNC_RESPONSE,
            Duration::from_secs(5),
        )?;
        let t4 = clock::client_nanos();
        let measurement = clock::measure(response.t1, response.t2, response.t3, t4);
        if let Ok(mut clock) = self.clock.lock() {
            if response.clock_rate != 0 {
                clock.set_rate(response.clock_rate);
            }
            clock.apply(measurement, response.session_ticks, t4);
        }
        Ok(())
    }

    /// Send `jam.stream_unsubscribe { all }`: drop everything and stay in
    /// control of what comes back.
    fn silence_ingress(&mut self, signaling: &mut SignalingClient, jam_id: &JamId) -> Result<()> {
        let ack: StreamUnsubscribed = signaling.request(
            message::JAM_STREAM_UNSUBSCRIBE,
            &StreamUnsubscribeRequest {
                jam_id: jam_id.as_str().to_string(),
                stream_ids: Vec::new(),
                all: true,
            },
            message::JAM_STREAM_UNSUBSCRIBED,
            self.config.connect_timeout,
        )?;
        self.apply_unsubscribed(ack);
        Ok(())
    }

    /// Make what the server sends match what the host asked for.
    ///
    /// The wire traffic is the diff, not the whole set: a project that re-points
    /// one track sends one message about one stream. The exception is the switch
    /// out of automatic ingress, where the first `subscribe` has to restate
    /// everything wanted — it replaces what the server was choosing, and a
    /// stream left out of it would keep arriving.
    fn reconcile_ingress(&mut self, signaling: &mut SignalingClient, jam_id: &JamId) -> Result<()> {
        self.ingress_dirty = false;

        if self.ingress == JamIngress::Everything {
            if self.server_mode == SubscriptionMode::Auto {
                return Ok(());
            }
            let ack: StreamSubscribed = signaling.request(
                message::JAM_STREAM_SUBSCRIBE,
                &StreamSubscribeRequest {
                    jam_id: jam_id.as_str().to_string(),
                    stream_ids: Vec::new(),
                    all: true,
                },
                message::JAM_STREAM_SUBSCRIBED,
                self.config.connect_timeout,
            )?;
            self.apply_subscribed(ack);
            return Ok(());
        }

        // Only what the room actually has can be subscribed. The rest stays in
        // `wanted` and attaches when it appears.
        let present: BTreeSet<StreamId> = self
            .registry
            .streams()
            .into_iter()
            .map(|stream| stream.id.clone())
            .collect();
        let want: BTreeSet<StreamId> = self.wanted.intersection(&present).cloned().collect();

        if want.is_empty() {
            if self.server_mode == SubscriptionMode::Explicit && self.subscribed.is_empty() {
                return Ok(());
            }
            return self.silence_ingress(signaling, jam_id);
        }

        if self.server_mode == SubscriptionMode::Explicit {
            let drop: Vec<StreamId> = self.subscribed.difference(&want).cloned().collect();
            if !drop.is_empty() {
                let ack: StreamUnsubscribed = signaling.request(
                    message::JAM_STREAM_UNSUBSCRIBE,
                    &StreamUnsubscribeRequest {
                        jam_id: jam_id.as_str().to_string(),
                        stream_ids: drop.iter().map(|id| id.as_str().to_string()).collect(),
                        all: false,
                    },
                    message::JAM_STREAM_UNSUBSCRIBED,
                    self.config.connect_timeout,
                )?;
                self.apply_unsubscribed(ack);
            }
        }

        let add: Vec<StreamId> = if self.server_mode == SubscriptionMode::Auto {
            want.iter().cloned().collect()
        } else {
            want.difference(&self.subscribed).cloned().collect()
        };
        if add.is_empty() {
            return Ok(());
        }
        let ack: StreamSubscribed = signaling.request(
            message::JAM_STREAM_SUBSCRIBE,
            &StreamSubscribeRequest {
                jam_id: jam_id.as_str().to_string(),
                stream_ids: add.iter().map(|id| id.as_str().to_string()).collect(),
                all: false,
            },
            message::JAM_STREAM_SUBSCRIBED,
            self.config.connect_timeout,
        )?;
        self.apply_subscribed(ack);
        Ok(())
    }

    /// Fold a `jam.stream_subscribed` reply in.
    fn apply_subscribed(&mut self, ack: StreamSubscribed) {
        self.server_mode = ack.mode;
        if ack.mode == SubscriptionMode::Auto {
            // Under automatic ingress the set is the server's, and the reply
            // carries none. Formats for the whole room follow as ordinary
            // `audio.format_selected` events, so there is nothing to clear.
            self.subscribed.clear();
            self.emit(JamEvent::IngressChanged {
                mode: ack.mode,
                streams: Vec::new(),
                refused: ack.refused,
            });
            return;
        }
        let held: BTreeSet<StreamId> = ack.stream_ids.iter().cloned().map(StreamId::new).collect();
        self.retire_formats(self.subscribed.difference(&held).cloned().collect());
        self.subscribed = held;
        self.settle_ingress(ack.mode, ack.refused);
    }

    /// Fold a `jam.stream_unsubscribed` reply in.
    fn apply_unsubscribed(&mut self, ack: StreamUnsubscribed) {
        self.server_mode = ack.mode;
        let held: BTreeSet<StreamId> = ack.stream_ids.iter().cloned().map(StreamId::new).collect();
        // `dropped` is what this message actually stopped, which is what has to
        // be released; diffing against the reported set would miss a stream the
        // server had subscribed under automatic ingress and this client never
        // named.
        let mut retire: Vec<StreamId> = ack.dropped.iter().cloned().map(StreamId::new).collect();
        for gone in self.subscribed.difference(&held) {
            if !retire.contains(gone) {
                retire.push(gone.clone());
            }
        }
        self.retire_formats(retire);
        self.subscribed = held;
        self.settle_ingress(ack.mode, Vec::new());
    }

    /// A stream that is no longer subscribed stops arriving. Clearing the
    /// negotiated format is what takes it out of the media table; the sink is
    /// told so it can release the ring and go silent rather than repeat its
    /// last block.
    fn retire_formats(&mut self, streams: Vec<StreamId>) {
        for stream in streams {
            if self.registry.clear_stream_format(&stream) {
                self.options.sink.stream_ended(&stream);
            }
        }
    }

    fn settle_ingress(&mut self, mode: SubscriptionMode, refused: Vec<SubscriptionRefusal>) {
        self.sync_subscriptions();
        self.publish_registry_snapshot();
        let streams: Vec<StreamId> = self.subscribed.iter().cloned().collect();
        self.emit(JamEvent::IngressChanged {
            mode,
            streams,
            refused,
        });
    }

    /// Tell the media threads which aliases they are receiving and in what
    /// format. Only streams the server has selected a format for are listed:
    /// anything else is audio this client is not subscribed to.
    fn sync_subscriptions(&mut self) {
        let mut table = std::collections::HashMap::new();
        for stream in self.registry.streams() {
            let Some(format) = stream.format else {
                continue;
            };
            table.insert(
                stream.alias.0,
                SubscribedStream {
                    id: stream.id.clone(),
                    format,
                    latency: stream.summary.latency,
                },
            );
        }
        self.media_shared.set_streams(table);
    }

    fn leave_quietly(&mut self, signaling: &mut SignalingClient, jam_id: &JamId) {
        let request = JamLeaveRequest {
            jam_id: jam_id.as_str().to_string(),
        };
        let _ = signaling.request::<protocol::JamLeft, _>(
            message::JAM_LEAVE,
            &request,
            message::JAM_LEFT,
            Duration::from_millis(500),
        );
        signaling.close();
        self.teardown_media();
    }

    fn teardown_media(&mut self) {
        if let Some(mut media) = self.media.take() {
            media.shutdown();
        }
        self.media_shared
            .set_streams(std::collections::HashMap::new());
    }

    // ── reporting ───────────────────────────────────────────────────────────

    fn set_state(&mut self, state: JamState) {
        if let Ok(mut guard) = self.state.write() {
            if *guard == state {
                return;
            }
            *guard = state;
        }
        self.update_snapshot(move |snapshot| snapshot.state_label = state.label());
        self.emit(JamEvent::State(state));
    }

    fn emit(&self, event: JamEvent) {
        // A caller that has stopped draining is not a reason to fail; the
        // snapshot still carries the current truth.
        let _ = self.events.send(event);
    }

    fn record_error(&mut self, error: &JamError, fatal: bool) {
        let message = error.user_message();
        let detail = error.to_string();
        self.update_snapshot({
            let message = message.clone();
            move |snapshot| snapshot.last_error = Some(message.clone())
        });
        self.emit(JamEvent::Error {
            message,
            detail,
            fatal,
        });
    }

    fn update_snapshot<F: FnOnce(&mut JamSnapshot)>(&self, edit: F) {
        if let Ok(mut guard) = self.snapshot.write() {
            edit(&mut guard);
        }
    }

    fn publish_registry_snapshot(&self) {
        let participants: Vec<ParticipantSummary> = self
            .registry
            .participants()
            .into_iter()
            .map(|participant| participant.summary.clone())
            .collect();
        let streams: Vec<StreamSummary> = self
            .registry
            .streams()
            .into_iter()
            .map(|stream| stream.summary.clone())
            .collect();
        let formats: Vec<(StreamId, AudioFormat)> = self
            .registry
            .streams()
            .into_iter()
            .filter_map(|stream| stream.format.map(|format| (stream.id.clone(), format)))
            .collect();
        self.update_snapshot(move |snapshot| {
            snapshot.participants = participants;
            snapshot.streams = streams;
            snapshot.formats = formats;
        });
    }

    fn publish_runtime_snapshot(&self) {
        let transport_stats = self
            .media
            .as_ref()
            .map(|media| media.transport_stats())
            .unwrap_or_default();
        let mut stream_stats: Vec<(StreamId, StreamRuntimeStats)> =
            self.media_shared.all_stats().into_iter().collect();
        stream_stats.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));

        let Ok(clock) = self.clock.lock() else {
            return;
        };
        let rtt = clock.rtt_millis();
        let offset = clock.offset_millis();
        let drift = clock.drift_ppm();
        let locked = clock.locked();
        drop(clock);
        self.update_snapshot(move |snapshot| {
            snapshot.transport_stats = transport_stats;
            snapshot.stream_stats = stream_stats;
            snapshot.rtt_ms = rtt;
            snapshot.clock_offset_ms = offset;
            snapshot.clock_drift_ppm = drift;
            snapshot.clock_locked = locked;
        });
    }
}

/// Why a session attempt ended.
enum SessionEnd {
    /// The caller asked to leave.
    Left,
    /// The worker is stopping.
    Shutdown,
    /// The jam ended for everybody.
    JamClosed(String),
    /// The connection went away and a reconnect is worth trying.
    Dropped,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_report_liveness_and_finality_explicitly() {
        assert!(JamState::Connected.live());
        assert!(!JamState::Reconnecting.live());
        assert!(JamState::Closed.terminal());
        assert!(JamState::Failed.terminal());
        assert!(!JamState::Disconnected.terminal());
        assert_eq!(JamState::Reconnecting.label(), "Reconnecting");
    }

    #[test]
    fn studio_capabilities_offer_pcm_only_and_the_smallest_frames_first() {
        let caps = JamSessionOptions::studio_capabilities();
        assert_eq!(caps.codecs.len(), 1);
        let pcm = &caps.codecs[0];
        assert_eq!(pcm.codec, AudioCodec::Pcm);
        assert!(pcm.bitrates.is_empty(), "no AAC bitrates are claimed");
        assert!(pcm.formats.contains(&SampleFormat::F32Le));
        assert!(pcm.sample_rates.contains(&48000));
        assert!(pcm.sample_rates.contains(&192000));
        assert_eq!(pcm.frame_sizes.first().copied(), Some(128));
        // Every layout up to the ceiling, so this client can be offered a
        // multitrack take as well as publish one.
        assert!(pcm.channels.contains(&1));
        assert!(pcm.channels.contains(&2));
        assert_eq!(
            pcm.channels.last().copied(),
            Some(MAX_STREAM_CHANNELS as i32)
        );
        // But the session-wide frame sizes stay sized for stereo: the server
        // picks the smallest both sides offer, so a short one listed here would
        // raise the packet rate of every ordinary stream.
        assert!(
            !pcm.frame_sizes.contains(&32),
            "a wide stream states its own frame size per publish instead"
        );
    }

    #[test]
    fn a_wide_stream_offers_the_largest_frame_size_that_still_fits_a_datagram() {
        use crate::bridge::{datagram_frame_sizes, JamPublishRequest};

        // 16 channels of 16-bit is 32 bytes a frame: only 32 samples fit.
        let sixteen = datagram_frame_sizes(16, SampleFormat::S16Le);
        assert_eq!(sixteen, vec![32]);
        // 8 channels of the same is 16 bytes, so 64 fits and is preferred.
        assert_eq!(datagram_frame_sizes(8, SampleFormat::S16Le), vec![64, 32]);
        // And 32-bit float at sixteen channels fits nothing at all, which is
        // what makes the request unpublishable rather than silently oversized.
        assert!(datagram_frame_sizes(16, SampleFormat::F32Le).is_empty());

        let request = JamPublishRequest::multitrack(
            "Studio Multitrack",
            vec!["trk_1".to_string(), "trk_2".to_string()],
            &["Drums".to_string(), "Bass".to_string()],
            SampleFormat::S16Le,
        );
        assert_eq!(request.channels, 4);
        assert_eq!(request.channel_labels[0], "Drums L");
        assert_eq!(request.channel_labels[3], "Bass R");
        // Four channels of 16-bit is 8 bytes a frame, so 128 samples is
        // 1024 bytes and the largest of the conventional sizes that fits.
        assert_eq!(
            request.frame_sizes,
            vec![128],
            "one size only, so the server's smallest-wins rule picks it"
        );
    }

    #[test]
    fn the_backoff_ladder_doubles_and_is_capped_by_configuration() {
        // The ladder itself, before jitter: 250 ms doubling, capped at 10 s.
        let cap = Duration::from_millis(10_000);
        let ladder: Vec<u128> = (0..8)
            .map(|attempt: u32| {
                BACKOFF_BASE
                    .checked_mul(1u32 << attempt.min(6))
                    .unwrap_or(cap)
                    .min(cap)
                    .as_millis()
            })
            .collect();
        assert_eq!(
            ladder,
            vec![250, 500, 1000, 2000, 4000, 8000, 10_000, 10_000]
        );
    }

    #[test]
    fn a_publish_request_carries_channel_labels_a_receiver_can_render() {
        let request = JamPublishRequest::stereo(
            "Guitar",
            crate::bridge::JamPublishSourceKind::Track {
                track_id: "trk_1".to_string(),
            },
        );
        assert_eq!(request.channels, 2);
        assert_eq!(request.channel_labels, vec!["L", "R"]);
        assert_eq!(request.source.tag(), "track");
    }
}
