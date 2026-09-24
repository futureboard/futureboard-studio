//! Audio Jam inside Futureboard Studio.
//!
//! ```txt
//! Studio UI (GPUI)
//!   │
//!   ├─ JamController ──▶ sphere-jam-client  (REST, signaling, media, clock)
//!   │        │
//!   │        ├─ JamEngineSink   ──▶ SphereDirectAudioEngine jam bus ──▶ track
//!   │        └─ JamEngineSource ◀── SphereDirectAudioEngine jam bus ◀── master
//!   │
//!   └─ Audio Connections ──▶ jam streams appear as input ports
//! ```
//!
//! Three decisions shape everything here.
//!
//! **The engine keeps the device.** A jam is a producer and consumer of buffers
//! the existing engine already fills and drains. It opens no WASAPI, CoreAudio
//! or ALSA client of its own, so there is nothing to contend for and no second
//! clock to reconcile.
//!
//! **A remote stream is an input port.** Routing a performer to a track goes
//! through the same Audio Connections layer hardware does — stable ids,
//! non-destructive device loss, one place that maps logical to physical. The
//! only difference is the device id prefix; see
//! [`DirectAudio::JAM_DEVICE_PREFIX`].
//!
//! **The jam is an output device too.** An enabled output bus bound to the
//! jam's send ports is a publish, named after the bus; see [`egress`].
//!
//! **The routing decides what arrives.** Because a remote stream is only
//! audible through an input port, a stream nobody routed was received, decoded
//! and thrown away. So the registry drives the subscription: what this Studio
//! asks the server to send is exactly what some enabled input connection binds.
//! See [`ingress`].
//!
//! **Identity is the account, never the username.** A saved routing stores a
//! user id; the display name follows whatever that account calls itself today.
//!
//! Nothing in this module is called from an audio callback.

pub mod egress;
pub mod ingress;
pub mod publish;
pub mod quality;
pub mod resample;
pub mod sink;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use sphere_jam_client::api::{CreateInviteRequest, CreateJamRequest, JamApiClient};
use sphere_jam_client::bridge::{JamPublishRequest, JamPublishSourceKind};
use sphere_jam_client::clock::SessionClock;
use sphere_jam_client::config::JamConfig;
use sphere_jam_client::credentials::{JamCredentialProvider, SharedCredentials};
use sphere_jam_client::error::{JamError, Result};
use sphere_jam_client::ids::{JamId, StreamId, UserId};
use sphere_jam_client::protocol::{ParticipantSummary, StreamSummary};
use sphere_jam_client::session::{JamEvent, JamSession, JamSessionOptions, JamSnapshot, JamState};
// What a host takes from the room is the host's choice, so the type that names
// it belongs to this facade as much as the controller does.
pub use sphere_jam_client::session::JamIngress;
use DirectAudio::engine::SharedState;
use DirectAudio::jam_bus::{
    jam_device_id, publish_key_track, PUBLISH_KEY_LIVE_INPUT, PUBLISH_KEY_MASTER,
    PUBLISH_KEY_MONITOR, PUBLISH_KEY_MULTITRACK,
};

pub use egress::{JamSend, JamSendTap};
// The façade owns the error type too. A caller that reaches this module for the
// controller should not also have to depend on the jam client crate to name
// what the controller returns.
pub use sphere_jam_client::error::{JamError as Error, Result as JamResult};

use crate::audio_connections::{AudioConnectionDirection, AvailablePort, AvailablePorts};

pub use publish::JamEngineSource;
pub use quality::{JamPublishQuality, JamStreamMode, StreamCost, SAMPLE_FORMATS, SAMPLE_RATES};
pub use sink::JamEngineSink;

/// The names this Studio's own streams carry in the room.
///
/// Fixed rather than derived from the project, because a name is also the key a
/// reconnect re-attaches by: `JamSession` adopts an existing stream of the same
/// name rather than minting a second one, so a name that followed the project
/// title would strand the old stream every time somebody renamed a song.
pub const MASTER_STREAM_NAME: &str = "Studio Master";
/// See [`MASTER_STREAM_NAME`].
pub const MULTITRACK_STREAM_NAME: &str = "Studio Multitrack";
/// See [`MASTER_STREAM_NAME`]. The live input pair, sent as one stream.
pub const LIVE_INPUT_STREAM_NAME: &str = "Live Input";

/// The account token Studio already holds.
///
/// The jam client never learns a password and never runs a sign-in flow. It
/// asks for a bearer token at the moment it needs one, so signing out in
/// Studio takes effect on the next jam call rather than on the next restart.
struct StudioCredentials;

impl JamCredentialProvider for StudioCredentials {
    fn access_token(&self) -> Result<String> {
        crate::auth::session_token()
            .ok_or_else(|| JamError::Auth("no Futureboard account is signed in".to_string()))
    }

    fn account_hint(&self) -> Option<String> {
        crate::auth::current_profile().map(|profile| profile.id)
    }
}

/// One remote stream, flattened for the UI and for the routing layer.
#[derive(Debug, Clone, Default)]
pub struct JamStreamView {
    pub stream_id: String,
    pub user_id: String,
    pub device_id: String,
    /// The `@handle` of the publisher, for display only.
    pub handle: String,
    pub display_name: String,
    /// The publisher's profile picture, as the account service serves it.
    /// Empty when they have none. Display only; fetch it with
    /// [`crate::account::prefetch_profile_picture`].
    pub avatar_url: String,
    pub stream_name: String,
    pub channels: usize,
    pub channel_labels: Vec<String>,
    pub sample_rate: i32,
    pub codec: String,
    /// Whether the server has resolved a format for this receiver — that is,
    /// whether audio is actually on its way.
    pub receiving: bool,
    /// Peak since the last UI read, linear.
    pub peak: f32,
    /// Round trip of the packets carrying it, in milliseconds.
    pub rtt_ms: f64,
    /// Whether a track in *this* project listens to it.
    ///
    /// The difference between "in the room" and "audible here", and the one the
    /// panel could not previously show: an unrouted stream is not subscribed,
    /// so it is silent for a reason that has nothing to do with the network.
    pub routed: bool,
    /// Whether its ring is still filling its cushion rather than playing.
    pub buffering: bool,
    /// Underruns plus overruns on its ring since it was bound — how often the
    /// network failed to keep the cushion.
    pub dropouts: u64,
}

/// One stream this Studio is putting into the room, as the panel shows it.
///
/// Built from what the engine is actually doing rather than from what was
/// asked for: `level` is the peak of audio that really left the tap, so a send
/// bound to something nothing feeds reads differently from a working one.
#[derive(Debug, Clone, PartialEq)]
pub struct JamSendView {
    /// The publish key — the identity, stable across a rename.
    pub key: String,
    /// What the room sees.
    pub name: String,
    /// Which engine tap it comes from, in words.
    pub tap: String,
    /// Peak of what left since the last UI read, linear.
    pub level: f32,
}

impl JamStreamView {
    /// What a track input menu shows: `@hachi224 · Guitar`.
    pub fn menu_label(&self) -> String {
        if self.handle.is_empty() {
            self.stream_name.clone()
        } else {
            format!("{} · {}", self.handle, self.stream_name)
        }
    }

    /// The Audio Connections device id this stream is addressed by.
    pub fn device_id(&self) -> String {
        jam_device_id(&self.stream_id)
    }
}

/// The whole jam, as a UI frame sees it.
///
/// Published once per poll rather than read live, so a panel render never takes
/// a lock the network threads also want.
#[derive(Debug, Clone, Default)]
pub struct JamUiState {
    pub configured: bool,
    pub signed_in: bool,
    pub state_label: String,
    pub connected: bool,
    pub jam_id: String,
    pub jam_name: String,
    pub public_id: String,
    pub join_url: String,
    pub region_label: String,
    pub transport_label: String,
    pub rtt_ms: f64,
    pub clock_offset_ms: f64,
    pub clock_drift_ppm: f64,
    pub clock_locked: bool,
    pub packets_in: u64,
    pub packets_out: u64,
    pub participants: Vec<ParticipantSummary>,
    pub streams: Vec<JamStreamView>,
    /// Streams this Studio is publishing, by key.
    pub publishing: Vec<String>,
    /// The same streams as the panel shows them: name, tap, and whether audio
    /// is really leaving.
    pub sending: Vec<JamSendView>,
    pub last_error: Option<String>,
    /// The most recent invite link minted from this Studio, shown once so it
    /// can be copied. Never persisted.
    pub invite_link: Option<String>,
    /// The wire format this Studio publishes with, and what it publishes.
    pub quality: JamPublishQuality,
    /// Tracks the multitrack stream carries, in channel-pair order.
    pub multitrack_tracks: Vec<(String, String)>,
}

impl JamUiState {
    /// Every stream belonging to one account, in menu order.
    pub fn streams_for_user(&self, user_id: &str) -> Vec<&JamStreamView> {
        self.streams
            .iter()
            .filter(|stream| stream.user_id == user_id)
            .collect()
    }

    /// Accounts present in the room, in display order, each with its streams.
    pub fn by_participant(&self) -> Vec<(&ParticipantSummary, Vec<&JamStreamView>)> {
        self.participants
            .iter()
            .map(|participant| {
                let streams = self
                    .streams
                    .iter()
                    .filter(|stream| {
                        stream.user_id == participant.user.id
                            && stream.device_id == participant.device_id
                    })
                    .collect();
                (participant, streams)
            })
            .collect()
    }
}

/// The published UI state. Separate from the controller so the routing layer
/// can read it without ever waiting on the network.
fn ui_state() -> &'static RwLock<JamUiState> {
    static STATE: OnceLock<RwLock<JamUiState>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(JamUiState::default()))
}

/// The current jam, for a UI frame.
pub fn snapshot() -> JamUiState {
    ui_state()
        .read()
        .map(|state| state.clone())
        .unwrap_or_default()
}

/// Who is behind a jam device id, for a menu row that has to show a face
/// rather than a device name.
#[derive(Debug, Clone, PartialEq)]
pub struct JamStreamIdentity {
    /// The person, as they would be introduced: their display name when the
    /// room knows one, else their handle.
    pub person: String,
    /// Profile picture URL, empty when they have none.
    pub avatar_url: String,
    /// Two characters to stand in for the picture while it is downloading, or
    /// for good if they have none.
    pub monogram: String,
}

/// The person publishing the stream a jam device id names.
///
/// `None` for a hardware device, for the jam *send* device, and for a stream
/// that has left the room — all three are cases where there is no face to show
/// and the caller falls back to an icon.
pub fn stream_identity(device_id: &str) -> Option<JamStreamIdentity> {
    let stream_id = DirectAudio::jam_bus::jam_stream_id(device_id)?;
    let state = snapshot();
    let stream = state
        .streams
        .iter()
        .find(|stream| stream.stream_id == stream_id)?;
    let person = if stream.display_name.trim().is_empty() {
        stream.handle.clone()
    } else {
        stream.display_name.clone()
    };
    Some(JamStreamIdentity {
        monogram: monogram_for(&person),
        person,
        avatar_url: stream.avatar_url.clone(),
    })
}

/// Initials for a name, at most two characters.
///
/// Word initials when the name has words to take them from ("Aria Kim" → AK),
/// otherwise the first two characters, which is the honest answer for a handle
/// like `@nut` and for scripts that do not have word-initial conventions.
fn monogram_for(person: &str) -> String {
    let trimmed = person.trim().trim_start_matches('@');
    let initials: String = trimmed
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(2)
        .collect();
    if initials.chars().count() >= 2 {
        return initials.to_uppercase();
    }
    trimmed.chars().take(2).collect::<String>().to_uppercase()
}

/// Jam streams as Audio Connections input ports.
///
/// This is what makes a remote performer selectable in the same Input menu a
/// hardware channel is. One port per channel, named the way the publisher
/// labelled it, under a device id that carries the stream's immutable id.
pub fn available_ports() -> AvailablePorts {
    let state = snapshot();
    let mut ports = Vec::new();
    for stream in &state.streams {
        let device_id = stream.device_id();
        let device_name = stream.menu_label();
        for channel in 0..stream.channels.max(1) {
            let port_name = stream
                .channel_labels
                .get(channel)
                .cloned()
                .unwrap_or_else(|| match (stream.channels, channel) {
                    (1, _) => "Mono".to_string(),
                    (2, 0) => "L".to_string(),
                    (2, 1) => "R".to_string(),
                    _ => format!("Ch {}", channel + 1),
                });
            ports.push(AvailablePort {
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                port_name,
                port_index: channel as u32,
                direction: AudioConnectionDirection::Input,
            });
        }
    }
    AvailablePorts { ports }.merge(egress::available_ports())
}

/// The process-wide controller.
///
/// One jam per Studio: a second one is a second application, which keeps event
/// ordering, the resume token and the audio bus unambiguous.
fn controller() -> &'static Mutex<Option<JamController>> {
    static CONTROLLER: OnceLock<Mutex<Option<JamController>>> = OnceLock::new();
    CONTROLLER.get_or_init(|| Mutex::new(None))
}

/// Install the controller once the engine exists. Idempotent.
///
/// Installing also starts the poll thread, because the jam has to keep its
/// published state current whether or not the panel is open: tracks routed to a
/// remote performer keep playing after the window is closed, and the routing
/// layer reads the same snapshot to decide what is still selectable.
pub fn install(shared: Arc<SharedState>, engine: Option<DirectAudio::AudioEngine>) -> Result<()> {
    install_with_ingress(shared, engine, JamIngress::Routed)
}

/// Install the controller, choosing what arrives from the room.
///
/// The two hosts want opposite defaults, and neither is wrong:
///
/// * **Studio** takes only what a track is routed to ([`JamIngress::Routed`]).
///   A remote stream is audible through an input connection or not at all, so
///   taking the whole room would decode audio the callback never reads and make
///   a project with two routed tracks pay for a six-piece band.
/// * **A jam client** takes the room ([`JamIngress::Everything`]). There is no
///   project to route through, and a listener who has to ask for each performer
///   before hearing any of them has joined a room that is silent for a reason
///   nothing on screen explains.
pub fn install_with_ingress(
    shared: Arc<SharedState>,
    engine: Option<DirectAudio::AudioEngine>,
    ingress: JamIngress,
) -> Result<()> {
    let mut guard = controller()
        .lock()
        .map_err(|_| JamError::Session("the jam controller lock was poisoned".to_string()))?;
    if guard.is_some() {
        return Ok(());
    }
    let mut created = JamController::new(shared, engine)?;
    created.ingress = ingress;
    created.publish_ui_state();
    *guard = Some(created);
    drop(guard);
    spawn_poll_thread();
    Ok(())
}

/// How often the published state is refreshed while a jam is live. The rest of
/// Studio meters at 30 Hz; anything faster repaints for packets nobody can see
/// arriving.
const POLL_ACTIVE: std::time::Duration = std::time::Duration::from_millis(33);

/// How often it is refreshed while nothing is connected. A lock and an early
/// return four times a second is not worth optimising further, and it is what
/// notices the moment a jam starts.
const POLL_IDLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Keep the published jam state current, independently of any window.
fn spawn_poll_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("jam-poll".to_string())
        .spawn(|| {
            loop {
                let live = match controller().lock() {
                    Ok(mut guard) => match guard.as_mut() {
                        Some(controller) => {
                            controller.poll();
                            controller.state().live()
                        }
                        None => false,
                    },
                    // A poisoned lock means a panic elsewhere already took the
                    // controller down; there is nothing left to poll.
                    Err(_) => return,
                };
                std::thread::sleep(if live { POLL_ACTIVE } else { POLL_IDLE });
            }
        });
}

/// Whether a controller has been installed.
pub fn installed() -> bool {
    controller()
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

/// Run `edit` against the installed controller.
///
/// Returns an error rather than silently doing nothing when no controller
/// exists: a menu item that appears to work and does not is worse than one that
/// explains itself.
pub fn with_controller<T>(edit: impl FnOnce(&mut JamController) -> Result<T>) -> Result<T> {
    let mut guard = controller()
        .lock()
        .map_err(|_| JamError::Session("the jam controller lock was poisoned".to_string()))?;
    match guard.as_mut() {
        Some(controller) => edit(controller),
        None => Err(JamError::Session(
            "the audio engine is not running yet".to_string(),
        )),
    }
}

/// Drain jam events and refresh the published UI state.
///
/// Called once per UI frame. Returns the events so a caller can react to a
/// stream appearing — re-resolving track routing, for instance — without
/// polling the snapshot for differences.
pub fn poll() -> Vec<JamEvent> {
    let Ok(mut guard) = controller().lock() else {
        return Vec::new();
    };
    match guard.as_mut() {
        Some(controller) => controller.poll(),
        None => Vec::new(),
    }
}

/// The jam session's clock, when a session is running.
pub fn session_clock() -> Option<Arc<Mutex<SessionClock>>> {
    controller()
        .lock()
        .ok()?
        .as_ref()
        .map(|controller| Arc::clone(&controller.clock))
}

/// Owns the jam session and the two bridges into the engine.
pub struct JamController {
    config: JamConfig,
    credentials: SharedCredentials,
    shared: Arc<SharedState>,
    /// The engine, for the control-plane half of a track publish. `None` in a
    /// build with no engine running, where sharing a track is simply refused.
    engine: Option<DirectAudio::AudioEngine>,
    clock: Arc<Mutex<SessionClock>>,
    sink: Arc<JamEngineSink>,
    api: JamApiClient,
    session: Option<JamSession>,
    device_id: String,
    /// Publish keys currently bound in the engine bus, so leaving releases
    /// exactly what joining claimed.
    published_keys: Vec<String>,
    /// The last invite minted here, held only until the UI has shown it.
    invite_link: Option<String>,
    /// The wire format this Studio publishes with, and what it publishes.
    /// Session state: it is never written to a project.
    quality: JamPublishQuality,
    /// Tracks the multitrack stream carries, in channel-pair order.
    multitrack_tracks: Vec<(String, String)>,
    /// The jam streams this project's routing is pointed at, as last pushed to
    /// the session. Held so a routing recompile — which happens on every edit
    /// that can move a port — costs nothing unless a jam binding actually
    /// changed.
    routed_streams: BTreeSet<String>,
    /// What this host takes from the room. See [`install_with_ingress`].
    ingress: JamIngress,
    /// The jam sends this project's output routing names, keyed by bus id, as
    /// last pushed to the session. Kept across a leave so joining again
    /// restates them.
    sends: BTreeMap<String, JamSend>,
    last_error: Option<String>,
}

impl JamController {
    fn new(shared: Arc<SharedState>, engine: Option<DirectAudio::AudioEngine>) -> Result<Self> {
        let config = JamConfig::from_env()?;
        let credentials: SharedCredentials = Arc::new(StudioCredentials);
        let clock = Arc::new(Mutex::new(SessionClock::default()));
        let sink = Arc::new(JamEngineSink::new(Arc::clone(&shared), Arc::clone(&clock)));
        let api = JamApiClient::new(config.clone(), Arc::clone(&credentials))?;
        Ok(Self {
            config,
            credentials,
            shared,
            engine,
            clock,
            sink,
            api,
            session: None,
            device_id: device_id(),
            published_keys: Vec::new(),
            invite_link: None,
            quality: JamPublishQuality::default(),
            multitrack_tracks: Vec::new(),
            routed_streams: BTreeSet::new(),
            ingress: JamIngress::Routed,
            sends: BTreeMap::new(),
            last_error: None,
        })
    }

    pub fn config(&self) -> &JamConfig {
        &self.config
    }

    pub fn api(&self) -> &JamApiClient {
        &self.api
    }

    pub fn state(&self) -> JamState {
        self.session
            .as_ref()
            .map(|session| session.state())
            .unwrap_or(JamState::Disconnected)
    }

    /// Clear the last failure.
    ///
    /// Called at the head of every action a person takes, because an error is a
    /// report about an attempt and not a property of the room. Without this the
    /// banner outlives what it described: a failure during Create stayed on
    /// screen through a successful join, a successful invite and a live stream,
    /// which is worse than saying nothing — it tells somebody their working
    /// session is broken.
    fn clear_error(&mut self) {
        self.last_error = None;
    }

    /// Create a jam and join it. Returns the shareable code.
    pub fn create_and_join(&mut self, name: &str) -> Result<String> {
        self.clear_error();
        let created = self.api.create_jam(&CreateJamRequest {
            name: name.to_string(),
            region: self.config.preferred_region.wire_value().to_string(),
            ..Default::default()
        })?;
        let code = created.jam.public_id.clone();
        let jam_id = JamId::new(created.jam.id.clone());
        self.set_join_url(created.join_url.clone());
        self.join(jam_id.clone(), String::new())?;
        // Mint the invite here rather than waiting to be asked. Creating a room
        // is never the goal — handing somebody the link is — and making that a
        // second deliberate gesture is most of why starting a jam feels like a
        // procedure. A failure is not fatal: the room exists, the Invite button
        // still works, and the error goes where the panel already reads errors.
        // Against the id the REST call just returned, never the session's:
        // the join above is queued, not finished, so the session has no jam id
        // to give for some time yet.
        //
        // A failure here is logged, not raised. This invite is a convenience
        // the user did not ask for, the room it belongs to is up and working,
        // and the Invite button is still there — putting it in the error banner
        // reports a healthy session as a broken one, which is exactly what it
        // did.
        if let Err(error) = self.create_invite_for(&jam_id, "performer") {
            eprintln!("[jam] the opening invite could not be minted: {error}");
        }
        self.publish_ui_state();
        Ok(code)
    }

    /// Follow an invite link: exchange the secret, then join what it admitted
    /// this account to.
    pub fn join_with_invite(&mut self, code: &str, secret: &str) -> Result<()> {
        self.clear_error();
        let exchanged = self.api.exchange_invite(secret, code)?;
        let jam_id = JamId::new(exchanged.jam.id.clone());
        self.join(jam_id, exchanged.access_token)
    }

    /// Join whatever a pasted link or code names.
    ///
    /// Both shapes a person can actually be handed:
    ///
    /// * an invite link, `https://.../j/CODE#secret`, which carries a bearer
    ///   secret in its fragment and is exchanged for an access token;
    /// * a room link or bare code, which carries no secret and only works for
    ///   an account the jam already admits.
    ///
    /// The distinction is not cosmetic — an invite is how somebody who is *not*
    /// yet a member gets in — so the fragment decides which call is made rather
    /// than both being tried in turn. A link with no fragment that resolves to
    /// a jam this account cannot enter fails as a permission error, which is
    /// the truth, instead of as "invalid link".
    pub fn join_with_link(&mut self, link: &str) -> Result<()> {
        self.clear_error();
        let parsed = parse_jam_link(link)
            .ok_or_else(|| JamError::Config(format!("{link:?} is not a jam link or code")))?;
        match parsed.secret {
            Some(secret) => self.join_with_invite(&parsed.code, &secret),
            None => {
                let jam = self.api.jam_by_code(&parsed.code)?;
                let jam_id = JamId::new(jam.jam.id.clone());
                self.set_join_url(jam.join_url.clone());
                // No secret means no jam access token, and the server's rule is
                // explicit: the host gets in, a token gets in, an existing
                // member gets in, and everybody else is refused. Saying which
                // link this was is the difference between a refusal somebody
                // can act on and one that reads as the app being broken.
                self.join(jam_id, String::new()).map_err(room_link_refusal)
            }
        }
    }

    /// Join a jam this account is already a member of, or was invited to.
    pub fn join(&mut self, jam_id: JamId, access_token: String) -> Result<()> {
        if self.session.is_none() {
            let sink: Arc<dyn sphere_jam_client::JamAudioSink> =
                Arc::clone(&self.sink) as Arc<dyn sphere_jam_client::JamAudioSink>;
            let mut options = JamSessionOptions::new(self.device_id.clone(), sink);
            options.device_name = device_name();
            options.publish_sample_rate = PUBLISH_SAMPLE_RATE as i32;
            // Whose choice this is depends on the host — see
            // [`install_with_ingress`]. Studio routes; a jam client takes the
            // room so joining is audible without a second gesture.
            options.ingress = self.ingress;
            let session = JamSession::spawn_with_clock(
                self.config.clone(),
                Arc::clone(&self.credentials),
                options,
                Arc::clone(&self.clock),
            )?;
            self.session = Some(session);
        }
        let Some(session) = self.session.as_ref() else {
            return Err(JamError::Session("the jam worker is gone".to_string()));
        };
        // A freshly spawned worker knows nothing about this project's routing.
        // Restating it here rather than waiting for the next routing edit is
        // what makes a track already bound to a performer arrive on join.
        if !self.routed_streams.is_empty() {
            let wanted: Vec<StreamId> = self
                .routed_streams
                .iter()
                .map(|id| StreamId::new(id.clone()))
                .collect();
            session.subscribe(wanted)?;
        }
        session.join(jam_id, access_token)?;
        // Sends are queued the same way: the worker publishes them as soon as
        // the room exists, so an output bus already bound to the jam is live on
        // join without a routing edit to remind it.
        let sends: Vec<JamSend> = self.sends.values().cloned().collect();
        for send in &sends {
            if let Err(error) = self.publish_send(send) {
                self.last_error = Some(error.to_string());
            }
        }
        // A default that works. Joining a room and being inaudible — with the
        // remedy being an output bus bound to a port on a device you have to
        // know exists — is why this reads as hard to use. When the project says
        // nothing about what to send, send the mix; it is the one answer that
        // is right for every room, and one click switches it off.
        if self.published_keys.is_empty() {
            if let Err(error) = self.publish_master() {
                self.last_error = Some(error.to_string());
            }
        }
        Ok(())
    }

    /// Ask the server for exactly the streams this project's routing names.
    ///
    /// Called on every routing recompile, so it diffs first: an edit that moved
    /// a hardware port must not cost a round trip about jam streams that did not
    /// change. Streams no longer routed anywhere are unsubscribed, which is
    /// where the bandwidth is actually saved — see [`ingress`].
    pub fn set_routed_streams(&mut self, streams: &[String]) -> Result<()> {
        let wanted: BTreeSet<String> = streams.iter().cloned().collect();
        if wanted == self.routed_streams {
            return Ok(());
        }
        let added: Vec<StreamId> = wanted
            .difference(&self.routed_streams)
            .map(|id| StreamId::new(id.clone()))
            .collect();
        let dropped: Vec<StreamId> = self
            .routed_streams
            .difference(&wanted)
            .map(|id| StreamId::new(id.clone()))
            .collect();
        self.routed_streams = wanted;

        // Recorded even with no session open: the intent is the project's, and
        // joining later restates it.
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        if !dropped.is_empty() {
            session.unsubscribe(dropped)?;
        }
        if !added.is_empty() {
            session.subscribe(added)?;
        }
        Ok(())
    }

    /// The jam streams this project's routing is pointed at.
    pub fn routed_streams(&self) -> Vec<String> {
        self.routed_streams.iter().cloned().collect()
    }

    /// Make what this Studio publishes match the output buses bound to the jam.
    ///
    /// Diffed by bus: an unchanged send costs nothing, a renamed or re-laid-out
    /// one is republished (the name is the stream's identity in the room, and
    /// the layout is announced per stream), and a bus that is gone or disabled
    /// is unpublished - which is where an output bus's off switch actually
    /// stops the bandwidth. See [`egress`].
    pub fn set_sends(&mut self, wanted: &[JamSend]) -> Result<()> {
        let wanted = egress::by_connection(wanted.to_vec());
        if wanted == self.sends {
            return Ok(());
        }
        let mut first_error = None;
        let previous = std::mem::replace(&mut self.sends, wanted.clone());
        for (id, old) in &previous {
            if wanted.get(id) != Some(old) {
                self.unpublish_send(old);
            }
        }
        if self.session.is_some() {
            for (id, send) in &wanted {
                if previous.get(id) == Some(send) {
                    continue;
                }
                if let Err(error) = self.publish_send(send) {
                    first_error.get_or_insert(error);
                }
            }
        }
        self.publish_ui_state();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The jam sends this project's output routing names.
    pub fn sends(&self) -> Vec<JamSend> {
        self.sends.values().cloned().collect()
    }

    fn send_key(tap: JamSendTap) -> &'static str {
        match tap {
            JamSendTap::Master => PUBLISH_KEY_MASTER,
            JamSendTap::Monitor => PUBLISH_KEY_MONITOR,
            JamSendTap::LiveInput => PUBLISH_KEY_LIVE_INPUT,
        }
    }

    /// Publish one send: claim the engine tap, announce a stream named after
    /// the bus.
    fn publish_send(&mut self, send: &JamSend) -> Result<()> {
        let Some(session) = self.session.as_ref() else {
            return Err(JamError::Session("not in a jam".to_string()));
        };
        let key = Self::send_key(send.tap);
        // One stream per tap. The engine hands the same ring to every claim of
        // one key, and two streams draining one ring would each hear half of
        // it; refusing is the only honest answer, and the panel's own master
        // share is the usual other claimant.
        if self.published_keys.iter().any(|held| held == key) {
            return Err(JamError::Audio(format!(
                "{} is already being sent to the jam",
                match send.tap {
                    JamSendTap::Master => "the master mix",
                    JamSendTap::Monitor => "the monitor output",
                    JamSendTap::LiveInput => "the live input",
                }
            )));
        }
        if self.shared.jam_bus.bind_publish(key).is_none() {
            return Err(JamError::Audio(
                "no publish slot is free in the audio engine".to_string(),
            ));
        }
        self.published_keys.push(key.to_string());
        if send.tap == JamSendTap::Master {
            self.shared
                .jam_bus
                .set_master_click_published(self.quality.master_click);
        }

        let source = Arc::new(JamEngineSource::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.clock),
            key,
            self.quality.sample_rate,
        ));
        let kind = match send.tap {
            JamSendTap::Master => JamPublishSourceKind::Master,
            // The room does not need to know it came from a Control Room rather
            // than a master bus; what it needs is one stereo mix from this
            // Studio, named after the bus the user bound.
            JamSendTap::Monitor => JamPublishSourceKind::Master,
            JamSendTap::LiveInput => JamPublishSourceKind::HardwareInput {
                connection: send.connection_id.clone(),
            },
        };
        let mut request = if send.channels >= 2 {
            JamPublishRequest::stereo(send.name.clone(), kind)
        } else {
            JamPublishRequest::mono(send.name.clone(), kind)
        };
        request.sample_format = self.quality.sample_format;
        request.sample_rate = self.quality.sample_rate as i32;
        match session.publish(request, source) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.published_keys.retain(|held| held != key);
                self.shared.jam_bus.release_publish(key);
                Err(error)
            }
        }
    }

    fn unpublish_send(&mut self, send: &JamSend) {
        let key = Self::send_key(send.tap);
        // Only a send that holds the tap releases it. The panel's own master
        // share uses the same key, and an output bus being switched off must
        // not silence a share the user made somewhere else.
        if !self.published_keys.iter().any(|held| held == key) {
            return;
        }
        self.unpublish_named(&send.name);
        self.published_keys.retain(|held| held != key);
        self.shared.jam_bus.release_publish(key);
    }

    /// Mint an invite for the current jam. The link is a bearer secret and is
    /// returned once; it is never stored.
    pub fn create_invite(&mut self, role: &str) -> Result<String> {
        self.clear_error();
        let jam_id = self
            .current_jam_id()
            .ok_or_else(|| JamError::Session("not in a jam".to_string()))?;
        self.create_invite_for(&jam_id, role)
    }

    /// Mint an invite for a jam named explicitly.
    ///
    /// Split from [`Self::create_invite`] because of *when* the two know the
    /// jam. `current_jam_id` reads the session's snapshot, and the session
    /// joins asynchronously: `JamSession::join` queues a command and returns
    /// long before the worker has a room. A caller that has just created a jam
    /// over REST already holds its id and must not ask the session for one it
    /// cannot have yet — doing so failed with "not in a jam", left the invite
    /// unminted, and left the only shareable link the room link, which admits
    /// nobody who was not already a member. Including the host's own browser.
    fn create_invite_for(&mut self, jam_id: &JamId, role: &str) -> Result<String> {
        let created = self.api.create_invite(
            jam_id.as_str(),
            &CreateInviteRequest {
                role: role.to_string(),
                max_uses: 8,
                ..Default::default()
            },
        )?;
        self.invite_link = Some(created.link.clone());
        Ok(created.link)
    }

    /// Publish the master bus.
    ///
    /// The tap is on the master feed the engine already renders, before the
    /// Control Room touches it — a jam listener hears the mix, not this
    /// engineer's dim, mono or monitor inserts. Whether it also carries the
    /// metronome is [`JamPublishQuality::master_click`]; the engine decides
    /// where to mix the click relative to the tap, so neither answer costs the
    /// stream a resample or a second buffer.
    pub fn publish_master(&mut self) -> Result<()> {
        self.clear_error();
        let Some(session) = self.session.as_ref() else {
            return Err(JamError::Session("not in a jam".to_string()));
        };
        if self
            .published_keys
            .iter()
            .any(|held| held == PUBLISH_KEY_MASTER)
        {
            return Err(JamError::Audio(
                "the master mix is already being sent to the jam by an output bus".to_string(),
            ));
        }
        if self
            .shared
            .jam_bus
            .bind_publish(PUBLISH_KEY_MASTER)
            .is_none()
        {
            return Err(JamError::Audio(
                "no publish slot is free in the audio engine".to_string(),
            ));
        }
        self.published_keys.push(PUBLISH_KEY_MASTER.to_string());
        self.shared
            .jam_bus
            .set_master_click_published(self.quality.master_click);

        let source = Arc::new(JamEngineSource::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.clock),
            PUBLISH_KEY_MASTER,
            self.quality.sample_rate,
        ));
        let mut request =
            JamPublishRequest::stereo(MASTER_STREAM_NAME, JamPublishSourceKind::Master);
        request.sample_format = self.quality.sample_format;
        request.sample_rate = self.quality.sample_rate as i32;
        match session.publish(request, source) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.published_keys.retain(|key| key != PUBLISH_KEY_MASTER);
                self.shared.jam_bus.release_publish(PUBLISH_KEY_MASTER);
                Err(error)
            }
        }
    }

    /// Publish the live hardware input pair — "send my microphone".
    ///
    /// The one-gesture form of the same thing an output bus bound to the jam's
    /// Live Input ports does. It exists because binding a bus is a trip through
    /// Audio Connections, and a standalone client has no reason to own that
    /// window at all: sending your instrument is the entire point of joining.
    ///
    /// Binding the tap is also what makes the engine open the capture stream —
    /// see `sync_live_input_stream`, which treats a bound live-input publish as
    /// a request for the interface exactly as an armed track is.
    pub fn publish_live_input(&mut self) -> Result<()> {
        self.clear_error();
        let Some(session) = self.session.as_ref() else {
            return Err(JamError::Session("not in a jam".to_string()));
        };
        if self
            .published_keys
            .iter()
            .any(|held| held == PUBLISH_KEY_LIVE_INPUT)
        {
            return Err(JamError::Audio(
                "the live input is already being sent to the jam".to_string(),
            ));
        }
        if self
            .shared
            .jam_bus
            .bind_publish(PUBLISH_KEY_LIVE_INPUT)
            .is_none()
        {
            return Err(JamError::Audio(
                "no publish slot is free in the audio engine".to_string(),
            ));
        }
        self.published_keys.push(PUBLISH_KEY_LIVE_INPUT.to_string());

        let source = Arc::new(JamEngineSource::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.clock),
            PUBLISH_KEY_LIVE_INPUT,
            self.quality.sample_rate,
        ));
        let mut request = JamPublishRequest::stereo(
            LIVE_INPUT_STREAM_NAME,
            JamPublishSourceKind::HardwareInput {
                connection: PUBLISH_KEY_LIVE_INPUT.to_string(),
            },
        );
        request.sample_format = self.quality.sample_format;
        request.sample_rate = self.quality.sample_rate as i32;
        match session.publish(request, source) {
            Ok(()) => {
                self.publish_ui_state();
                Ok(())
            }
            Err(error) => {
                self.published_keys
                    .retain(|key| key != PUBLISH_KEY_LIVE_INPUT);
                self.shared.jam_bus.release_publish(PUBLISH_KEY_LIVE_INPUT);
                Err(error)
            }
        }
    }

    /// Stop publishing the live input.
    pub fn unpublish_live_input(&mut self) -> Result<()> {
        self.unpublish_named(LIVE_INPUT_STREAM_NAME);
        self.published_keys
            .retain(|key| key != PUBLISH_KEY_LIVE_INPUT);
        self.shared.jam_bus.release_publish(PUBLISH_KEY_LIVE_INPUT);
        self.publish_ui_state();
        Ok(())
    }

    /// Stop publishing the master bus.
    pub fn unpublish_master(&mut self) -> Result<()> {
        self.unpublish_named(MASTER_STREAM_NAME);
        self.published_keys.retain(|key| key != PUBLISH_KEY_MASTER);
        self.shared.jam_bus.release_publish(PUBLISH_KEY_MASTER);
        self.publish_ui_state();
        Ok(())
    }

    /// Share several tracks as one multitrack stream.
    ///
    /// `tracks` is the layout in order — the first entry fills channels 1-2,
    /// the second 3-4 — and each is a track id with the name the room should
    /// see for that pair. An empty list stops the stream.
    ///
    /// One stream, not one per track: a receiving Studio is dropping a take
    /// onto a timeline, and a single stream carries one capture base and one
    /// sequence, so every pair lands sample-aligned with the rest.
    pub fn publish_multitrack(&mut self, tracks: &[(String, String)]) -> Result<()> {
        if tracks.is_empty() {
            return self.stop_multitrack();
        }
        if self.session.is_none() {
            return Err(JamError::Session("not in a jam".to_string()));
        }
        if self.engine.is_none() {
            return Err(JamError::Audio(
                "the audio engine is not running".to_string(),
            ));
        }
        if tracks.len() > DirectAudio::jam_bus::MAX_MULTITRACK_PAIRS {
            return Err(JamError::Audio(format!(
                "a multitrack stream carries at most {} tracks",
                DirectAudio::jam_bus::MAX_MULTITRACK_PAIRS
            )));
        }

        let track_ids: Vec<String> = tracks.iter().map(|(id, _)| id.clone()).collect();
        let labels: Vec<String> = tracks.iter().map(|(_, name)| name.clone()).collect();
        let mut request = JamPublishRequest::multitrack(
            MULTITRACK_STREAM_NAME,
            track_ids.clone(),
            &labels,
            self.quality.sample_format,
        );
        // The layout has to fit a datagram before anything is claimed or
        // announced. Sixteen channels of 32-bit float is 64 bytes a frame, and
        // no frame length worth sending fits inside 1200 — the server would
        // refuse the stream, so refusing it here says why while the choice is
        // still the user's to change.
        if request.frame_sizes.is_empty() {
            return Err(JamError::Audio(format!(
                "{} tracks at {} do not fit one network packet — share fewer tracks, or choose a smaller bit depth",
                tracks.len(),
                quality::sample_format_label(self.quality.sample_format)
            )));
        }
        request.sample_rate = self.quality.sample_rate as i32;

        // Release before claiming: the engine fixes a stream's layout for the
        // life of a claim, so changing which tracks are shared is a republish.
        self.stop_multitrack()?;

        let Some(engine) = self.engine.as_ref() else {
            return Err(JamError::Audio(
                "the audio engine is not running".to_string(),
            ));
        };
        engine
            .set_multitrack_jam_publish(&track_ids)
            .map_err(|error| JamError::Audio(error.to_string()))?;
        let Some(session) = self.session.as_ref() else {
            let _ = engine.set_multitrack_jam_publish(&[]);
            return Err(JamError::Session("not in a jam".to_string()));
        };

        let source = Arc::new(JamEngineSource::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.clock),
            PUBLISH_KEY_MULTITRACK,
            self.quality.sample_rate,
        ));
        match session.publish(request, source) {
            Ok(()) => {
                self.published_keys.push(PUBLISH_KEY_MULTITRACK.to_string());
                self.multitrack_tracks = tracks.to_vec();
                Ok(())
            }
            Err(error) => {
                // The engine is already assembling blocks into a slot nothing
                // will read. Stop it rather than leaving a tap running for a
                // stream that was never announced.
                let _ = engine.set_multitrack_jam_publish(&[]);
                Err(error)
            }
        }
    }

    /// Stop the multitrack stream and release its slot.
    pub fn stop_multitrack(&mut self) -> Result<()> {
        self.unpublish_named(MULTITRACK_STREAM_NAME);
        if let Some(engine) = self.engine.as_ref() {
            let _ = engine.set_multitrack_jam_publish(&[]);
        }
        self.published_keys
            .retain(|key| key != PUBLISH_KEY_MULTITRACK);
        self.shared.jam_bus.release_publish(PUBLISH_KEY_MULTITRACK);
        self.multitrack_tracks.clear();
        Ok(())
    }

    /// Which tracks the multitrack stream carries, in channel-pair order.
    pub fn multitrack_tracks(&self) -> &[(String, String)] {
        &self.multitrack_tracks
    }

    /// The wire format this Studio publishes with.
    pub fn quality(&self) -> JamPublishQuality {
        self.quality.clone()
    }

    /// Replace the wire format.
    ///
    /// Depth, rate and layout are announced per stream, so a change reaches the
    /// room on the next publish rather than immediately. Nothing already live
    /// is republished behind the user's back: a stream whose format changed
    /// under a receiver would decode as noise until the receiver noticed.
    pub fn set_quality(&mut self, quality: JamPublishQuality) {
        self.shared
            .jam_bus
            .set_master_click_published(quality.master_click);
        self.quality = quality;
        self.publish_ui_state();
    }

    /// Share one track or bus over the jam.
    ///
    /// The engine claims the publish slot and starts writing that track's
    /// post-fader block into it; the jam client pulls from the other side. The
    /// two halves are deliberately separate calls, because the engine owns the
    /// slot and the jam owns the stream, and a failure in either must not leave
    /// the other half running.
    pub fn publish_track(&mut self, track_id: &str, name: &str) -> Result<()> {
        let Some(session) = self.session.as_ref() else {
            return Err(JamError::Session("not in a jam".to_string()));
        };
        let Some(engine) = self.engine.as_ref() else {
            return Err(JamError::Audio(
                "the audio engine is not running".to_string(),
            ));
        };
        engine
            .set_track_jam_publish(track_id, true)
            .map_err(|error| JamError::Audio(error.to_string()))?;

        let key = publish_key_track(track_id);
        let source = Arc::new(JamEngineSource::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.clock),
            key.clone(),
            PUBLISH_SAMPLE_RATE,
        ));
        match session.publish(
            JamPublishRequest::stereo(
                name.to_string(),
                JamPublishSourceKind::Track {
                    track_id: track_id.to_string(),
                },
            ),
            source,
        ) {
            Ok(()) => {
                self.published_keys.push(key);
                Ok(())
            }
            Err(error) => {
                // The engine is already writing into a slot nothing will read.
                // Stop it rather than leaving a tap running for a stream that
                // was never announced.
                let _ = engine.set_track_jam_publish(track_id, false);
                Err(error)
            }
        }
    }

    /// Stop sharing a track.
    pub fn unpublish_track(&mut self, track_id: &str) -> Result<()> {
        if let Some(engine) = self.engine.as_ref() {
            let _ = engine.set_track_jam_publish(track_id, false);
        }
        let key = publish_key_track(track_id);
        self.published_keys.retain(|held| held != &key);
        self.shared.jam_bus.release_publish(&key);
        Ok(())
    }

    /// Leave the jam, release every bus slot, and stop the worker.
    pub fn leave(&mut self) -> Result<()> {
        if let Some(session) = self.session.as_ref() {
            let _ = session.leave();
        }
        // Dropping the handle stops the worker and joins its threads.
        self.session = None;
        // Stop the engine assembling a stream nothing will read. Releasing the
        // slot alone would leave every shared track still staging its block
        // into it, which costs a memcpy per track per callback for a jam that
        // ended.
        if let Some(engine) = self.engine.as_ref() {
            let _ = engine.set_multitrack_jam_publish(&[]);
        }
        self.multitrack_tracks.clear();
        for key in self.published_keys.drain(..) {
            self.shared.jam_bus.release_publish(&key);
        }
        self.sink.clear();
        self.shared.jam_bus.release_all();
        self.publish_ui_state();
        Ok(())
    }

    /// Drain worker events and refresh the published UI state.
    pub fn poll(&mut self) -> Vec<JamEvent> {
        let events = self
            .session
            .as_ref()
            .map(|session| session.drain_events())
            .unwrap_or_default();
        for event in &events {
            if let JamEvent::Error { message, fatal, .. } = event {
                self.last_error = Some(message.clone());
                if *fatal {
                    for key in self.published_keys.drain(..) {
                        self.shared.jam_bus.release_publish(&key);
                    }
                }
            }
            if let JamEvent::StreamRemoved(stream) = event {
                self.shared.jam_bus.release_input(stream.as_str());
            }
            // A refused subscription is the one ingress failure a user can act
            // on: the performer is in the room, the track is pointed at them,
            // and the two formats do not meet. Silence with no explanation is
            // the failure mode this reports instead.
            if let JamEvent::IngressChanged { refused, .. } = event {
                if let Some(first) = refused.first() {
                    self.last_error = Some(format!(
                        "a jam stream could not be received: {}",
                        if first.message.is_empty() {
                            first.code.clone()
                        } else {
                            first.message.clone()
                        }
                    ));
                }
            }
        }
        self.publish_ui_state();
        events
    }

    fn current_jam_id(&self) -> Option<JamId> {
        self.session.as_ref()?.snapshot().jam_id
    }

    /// Withdraw the stream this Studio published under `name`.
    ///
    /// The room is asked by stream id, never by name — a name is a display
    /// label and another participant may well be publishing one that matches,
    /// so the lookup is narrowed to this device's own participant before an id
    /// is taken from it. Nothing to withdraw is not an error: leaving a jam and
    /// stopping a stream race, and the outcome either way is that the stream is
    /// gone.
    fn unpublish_named(&self, name: &str) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let snapshot = session.snapshot();
        let Some(mine) = snapshot.self_participant.as_ref().map(|p| p.id.clone()) else {
            return;
        };
        if let Some(stream) = snapshot
            .streams
            .iter()
            .find(|stream| stream.participant_id == mine && stream.name == name)
        {
            let _ = session.unpublish(StreamId::new(stream.id.clone()));
        }
    }

    fn set_join_url(&mut self, url: String) {
        if let Ok(mut state) = ui_state().write() {
            state.join_url = url;
        }
    }

    /// What this Studio is sending, as the panel shows it.
    ///
    /// Built from the publish keys the controller actually holds and the engine
    /// slots behind them, never from what was requested: a stream announced to
    /// the room but fed by a tap with nothing on it is the failure this view
    /// exists to make visible, and it is indistinguishable from a working one
    /// on any list built from intent.
    fn send_views(&self) -> Vec<JamSendView> {
        self.published_keys
            .iter()
            .map(|key| {
                let level = self
                    .shared
                    .jam_bus
                    .publish_slot_for(key)
                    .and_then(|index| self.shared.jam_bus.publish(index))
                    .map(|slot| slot.take_peak())
                    .unwrap_or(0.0);
                // A bus-bound send carries the user's own name for the stream;
                // a panel toggle has no bus, so it falls back to the tap's.
                let bus = self
                    .sends
                    .values()
                    .find(|send| Self::send_key(send.tap) == key);
                let (name, tap) = match bus {
                    Some(send) => (send.name.clone(), send_tap_label(send.tap).to_string()),
                    None => publish_key_labels(key),
                };
                JamSendView {
                    key: key.clone(),
                    name,
                    tap,
                    level,
                }
            })
            .collect()
    }

    /// Rebuild the published UI state from the session snapshot and the bus.
    fn publish_ui_state(&self) {
        let session_snapshot = self
            .session
            .as_ref()
            .map(|session| session.snapshot())
            .unwrap_or_default();
        let mut next = build_ui_state(&session_snapshot, &self.config, &self.shared);
        next.last_error = self
            .last_error
            .clone()
            .or_else(|| session_snapshot.last_error.clone());
        next.invite_link = self.invite_link.clone();
        next.publishing = self.published_keys.clone();
        next.sending = self.send_views();
        next.quality = self.quality.clone();
        next.multitrack_tracks = self.multitrack_tracks.clone();

        if let Ok(mut state) = ui_state().write() {
            // The join url comes from the REST response, which the session
            // snapshot never sees; carry it forward rather than blanking it.
            let join_url = if next.join_url.is_empty() {
                state.join_url.clone()
            } else {
                next.join_url.clone()
            };
            next.join_url = join_url;
            *state = next;
        }
    }
}

impl Drop for JamController {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// The rate Studio publishes at.
///
/// 48 kHz is the session clock's own rate and what every client is required to
/// handle, so it is the one choice that never fails to negotiate. The project
/// rate is unaffected: the publish tap converts.
pub const PUBLISH_SAMPLE_RATE: u32 = 48_000;

/// Assemble the flattened UI state from a session snapshot.
///
/// Split out so it can be tested without a network: everything it needs is in
/// the snapshot and the bus.
/// The words for one jam send tap.
fn send_tap_label(tap: JamSendTap) -> &'static str {
    match tap {
        JamSendTap::Master => "Master",
        JamSendTap::Monitor => "Control Room",
        JamSendTap::LiveInput => "Interface",
    }
}

/// Display name and tap for a publish key with no output bus behind it — the
/// panel's own Master / Arrangement toggles, and per-track shares.
fn publish_key_labels(key: &str) -> (String, String) {
    match key {
        PUBLISH_KEY_MASTER => ("Master mix".to_string(), "Master".to_string()),
        PUBLISH_KEY_MONITOR => ("Monitor output".to_string(), "Control Room".to_string()),
        PUBLISH_KEY_LIVE_INPUT => ("Live input".to_string(), "Interface".to_string()),
        PUBLISH_KEY_MULTITRACK => ("Arrangement".to_string(), "Tracks".to_string()),
        other => match other.strip_prefix("track:") {
            Some(track_id) => (format!("Track {track_id}"), "Track".to_string()),
            None => (other.to_string(), "Engine".to_string()),
        },
    }
}

fn build_ui_state(snapshot: &JamSnapshot, config: &JamConfig, shared: &SharedState) -> JamUiState {
    let mut streams = Vec::with_capacity(snapshot.streams.len());
    for summary in &snapshot.streams {
        let id = StreamId::new(summary.id.clone());
        let handle = snapshot
            .participants
            .iter()
            .find(|participant| participant.id == summary.participant_id)
            .map(|participant| participant.user.handle())
            .unwrap_or_default();
        let display_name = snapshot
            .participants
            .iter()
            .find(|participant| participant.id == summary.participant_id)
            .map(|participant| participant.user.label())
            .unwrap_or_default();
        let avatar_url = snapshot
            .participants
            .iter()
            .find(|participant| participant.id == summary.participant_id)
            .map(|participant| participant.user.avatar_url.clone())
            .unwrap_or_default();
        let receiving = snapshot
            .formats
            .iter()
            .any(|(stream_id, _)| stream_id == &id);
        let slot = shared
            .jam_bus
            .input_slot_for(&summary.id)
            .and_then(|index| shared.jam_bus.input(index));
        let peak = slot.map(|slot| slot.take_peak()).unwrap_or(0.0);
        // A slot that exists at all is one this project routed somewhere; the
        // rest of the room's streams are never bound and never subscribed.
        let routed = slot.is_some();
        let buffering = slot.map(|slot| slot.is_priming()).unwrap_or(false);
        let dropouts = slot
            .map(|slot| slot.underruns().saturating_add(slot.overruns()))
            .unwrap_or(0);

        // Start the download here rather than during render: the port menus and
        // the participant list both read the cache every frame and must never
        // be the thing that touches the network.
        if !avatar_url.is_empty() {
            crate::account::prefetch_profile_picture(&avatar_url);
        }

        streams.push(JamStreamView {
            stream_id: summary.id.clone(),
            user_id: summary.user_id.clone(),
            device_id: summary.device_id.clone(),
            handle,
            display_name,
            avatar_url,
            stream_name: summary.name.clone(),
            channels: summary.channels.max(0) as usize,
            channel_labels: channel_labels(summary),
            sample_rate: summary.sample_rate,
            codec: summary.codec.as_str().to_string(),
            receiving,
            peak,
            rtt_ms: snapshot.rtt_ms,
            routed,
            buffering,
            dropouts,
        });
    }

    JamUiState {
        configured: true,
        signed_in: crate::auth::session_token().is_some() || config.dev_token.is_some(),
        state_label: snapshot.state_label.to_string(),
        connected: snapshot.state_label == JamState::Connected.label(),
        jam_id: snapshot
            .jam_id
            .as_ref()
            .map(|id| id.as_str().to_string())
            .unwrap_or_default(),
        jam_name: snapshot.jam_name.clone(),
        public_id: snapshot.public_id.clone(),
        join_url: snapshot.join_url.clone(),
        region_label: region_label(snapshot),
        transport_label: snapshot
            .transport
            .map(|kind| kind.as_str().to_uppercase())
            .unwrap_or_else(|| "—".to_string()),
        rtt_ms: snapshot.rtt_ms,
        clock_offset_ms: snapshot.clock_offset_ms,
        clock_drift_ppm: snapshot.clock_drift_ppm,
        clock_locked: snapshot.clock_locked,
        packets_in: snapshot.transport_stats.packets_in,
        packets_out: snapshot.transport_stats.packets_out,
        participants: snapshot.participants.clone(),
        streams,
        publishing: Vec::new(),
        sending: Vec::new(),
        last_error: snapshot.last_error.clone(),
        invite_link: None,
        quality: JamPublishQuality::default(),
        multitrack_tracks: Vec::new(),
    }
}

/// Explain a refusal that a room link caused.
///
/// A room link carries no secret, so it admits only the host and people already
/// in the jam — which is the server's rule, not a client limitation, and there
/// is no call that mints a token for somebody who was never invited. The fix is
/// always the same and is never obvious from `forbidden`: ask for the invite
/// link, the one with a `#` in it.
fn room_link_refusal(error: JamError) -> JamError {
    match &error {
        JamError::Api(wire)
            if matches!(
                wire.code,
                sphere_jam_client::error::ErrorCode::Forbidden
                    | sphere_jam_client::error::ErrorCode::AccountRequired
            ) =>
        {
            JamError::Audio(
                "That is a room link, which only admits people already in the jam. \
                 Ask the host for an invite link — the one with a # in it."
                    .to_string(),
            )
        }
        _ => error,
    }
}

/// A jam link, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JamLink {
    /// The shareable code, e.g. `EWEFDN`.
    pub code: String,
    /// The invite secret from the fragment, when the link carries one.
    pub secret: Option<String>,
}

/// Read a pasted link or code.
///
/// Deliberately tolerant about the shell and strict about the payload: people
/// paste links with a trailing slash, with the scheme missing, wrapped in angle
/// brackets by a chat client, or they paste nothing but the six-character code
/// somebody read out to them. All of those name the same room. What it will not
/// do is guess: a string with no recognisable code returns `None` so the caller
/// can say the link is not a jam link, rather than making a request for a room
/// that was never named.
pub fn parse_jam_link(raw: &str) -> Option<JamLink> {
    let trimmed = raw
        .trim()
        .trim_matches(|c| c == '<' || c == '>' || c == '"');
    if trimmed.is_empty() {
        return None;
    }

    // The secret is a bearer credential and lives in the fragment precisely so
    // it never reaches a server log. Splitting it off first keeps it out of
    // every code path below.
    let (before_fragment, fragment) = match trimmed.split_once('#') {
        Some((before, after)) => (before, Some(after)),
        None => (trimmed, None),
    };
    let secret = fragment
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
        .map(str::to_string);

    let path = before_fragment
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(before_fragment);
    let path = path.split('?').next().unwrap_or(path);

    let code = match path.rsplit_once("/j/") {
        Some((_, code)) => code,
        // No `/j/` at all: either a bare code, or something that is not a jam
        // link. A bare code has no path separator in it, which is what tells
        // the two apart without guessing.
        None if !path.contains('/') => path,
        None => return None,
    };
    let code = code.trim_matches('/').trim();
    if code.is_empty()
        || !code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(JamLink {
        code: code.to_string(),
        secret,
    })
}

fn channel_labels(summary: &StreamSummary) -> Vec<String> {
    let channels = summary.channels.max(0) as usize;
    (0..channels)
        .map(|index| {
            summary
                .channel_metadata
                .iter()
                .find(|meta| meta.index as usize == index)
                .map(|meta| {
                    if meta.label.is_empty() {
                        meta.role.clone()
                    } else {
                        meta.label.clone()
                    }
                })
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| match (channels, index) {
                    (1, _) => "Mono".to_string(),
                    (2, 0) => "L".to_string(),
                    (2, 1) => "R".to_string(),
                    _ => format!("Ch {}", index + 1),
                })
        })
        .collect()
}

fn region_label(snapshot: &JamSnapshot) -> String {
    if !snapshot.region.display_name.is_empty() {
        snapshot.region.display_name.clone()
    } else if !snapshot.region.city.is_empty() {
        snapshot.region.city.clone()
    } else {
        snapshot.region.id.clone()
    }
}

/// This installation's device id, persisted beside the session file.
///
/// The server treats (account, device) as one participant, so reusing it across
/// restarts is what makes a relaunch re-attach instead of leaving a ghost
/// participant in the room until its resume window expires.
fn device_id() -> String {
    const FILE: &str = "jam-device-id";
    let path = crate::paths::FutureboardPaths::resolve()
        .app_data
        .join(FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing;
        }
    }
    let minted = sphere_jam_client::generate_device_id("studio");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &minted);
    minted
}

fn device_name() -> String {
    // The machine name is what another participant sees next to the account
    // when the same person is in the room on two devices.
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .map(|name| format!("Futureboard Studio · {name}"))
        .unwrap_or_else(|| "Futureboard Studio".to_string())
}

/// Whether a jam device id names a stream this client is currently receiving.
///
/// Used by the routing layer to render a saved route as "waiting for" rather
/// than as broken when the performer has not joined yet.
pub fn stream_is_live(device_id: &str) -> bool {
    let Some(stream_id) = DirectAudio::jam_stream_id(device_id) else {
        return false;
    };
    snapshot()
        .streams
        .iter()
        .any(|stream| stream.stream_id == stream_id && stream.receiving)
}

/// The account a saved jam route belongs to, for the "waiting for" label.
pub fn user_for_stream(device_id: &str) -> Option<UserId> {
    let stream_id = DirectAudio::jam_stream_id(device_id)?;
    snapshot()
        .streams
        .iter()
        .find(|stream| stream.stream_id == stream_id)
        .map(|stream| UserId::new(stream.user_id.clone()))
}

/// Diagnostic counters for the debug panel.
pub fn bus_diagnostics(shared: &SharedState) -> Vec<(String, u64, u64)> {
    shared
        .jam_bus
        .bound_inputs()
        .into_iter()
        .filter_map(|(stream_id, index)| {
            let slot = shared.jam_bus.input(index)?;
            Some((stream_id, slot.underruns(), slot.overruns()))
        })
        .collect()
}

/// The engine sample rate the jam branch converts into.
pub fn engine_rate(shared: &SharedState) -> u32 {
    let rate = shared.sample_rate.load(Ordering::Relaxed);
    if rate == 0 {
        48_000
    } else {
        rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_jam_client::protocol::{
        AudioCodec, AudioFormat, ChannelMetadata, SampleFormat, TransportKind, UserSummary,
    };

    fn participant(id: &str, user: &str, username: &str, device: &str) -> ParticipantSummary {
        ParticipantSummary {
            id: id.to_string(),
            user: UserSummary {
                id: user.to_string(),
                username: username.to_string(),
                display_name: username.to_string(),
                avatar_url: String::new(),
            },
            device_id: device.to_string(),
            connection_state: "connected".to_string(),
            ..Default::default()
        }
    }

    fn stream(id: &str, participant: &str, user: &str, device: &str, name: &str) -> StreamSummary {
        StreamSummary {
            id: id.to_string(),
            media_alias: 1,
            participant_id: participant.to_string(),
            user_id: user.to_string(),
            device_id: device.to_string(),
            name: name.to_string(),
            codec: AudioCodec::Pcm,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            channels: 2,
            channel_metadata: vec![
                ChannelMetadata {
                    index: 0,
                    label: String::new(),
                    role: "L".to_string(),
                },
                ChannelMetadata {
                    index: 1,
                    label: String::new(),
                    role: "R".to_string(),
                },
            ],
            active: true,
            ..Default::default()
        }
    }

    fn snapshot_with_one_guitar() -> JamSnapshot {
        JamSnapshot {
            state_label: JamState::Connected.label(),
            jam_id: Some(JamId::new("jam_1")),
            jam_name: "Saturday Session".to_string(),
            public_id: "J8KM4V".to_string(),
            participants: vec![participant("pcp_1", "usr_1", "hachi224", "studio-mac")],
            streams: vec![stream("str_1", "pcp_1", "usr_1", "studio-mac", "Guitar")],
            formats: vec![(
                StreamId::new("str_1"),
                AudioFormat {
                    codec: AudioCodec::Pcm,
                    sample_rate: 48_000,
                    channels: 2,
                    format: SampleFormat::F32Le,
                    bitrate: 0,
                    frame_samples: 128,
                },
            )],
            transport: Some(TransportKind::Udp),
            rtt_ms: 12.4,
            ..Default::default()
        }
    }

    #[test]
    fn a_room_snapshot_flattens_into_something_a_panel_can_render() {
        let shared = SharedState::default();
        let state = build_ui_state(&snapshot_with_one_guitar(), &JamConfig::default(), &shared);
        assert!(state.connected);
        assert_eq!(state.jam_name, "Saturday Session");
        assert_eq!(state.transport_label, "UDP");
        assert_eq!(state.streams.len(), 1);

        let guitar = &state.streams[0];
        assert_eq!(guitar.menu_label(), "@hachi224 · Guitar");
        assert_eq!(guitar.channel_labels, vec!["L", "R"]);
        assert!(guitar.receiving, "a selected format means audio is coming");
        assert_eq!(guitar.device_id(), "jam:str_1");
    }

    #[test]
    fn a_stream_with_no_selected_format_is_listed_but_not_marked_as_arriving() {
        let mut snapshot = snapshot_with_one_guitar();
        snapshot.formats.clear();
        let shared = SharedState::default();
        let state = build_ui_state(&snapshot, &JamConfig::default(), &shared);
        assert_eq!(state.streams.len(), 1);
        assert!(!state.streams[0].receiving);
    }

    #[test]
    fn streams_are_grouped_by_the_participant_that_publishes_them() {
        let mut snapshot = snapshot_with_one_guitar();
        // The same account on a second device, with its own stream.
        snapshot
            .participants
            .push(participant("pcp_2", "usr_1", "hachi224", "phone"));
        snapshot
            .streams
            .push(stream("str_2", "pcp_2", "usr_1", "phone", "Talkback"));

        let shared = SharedState::default();
        let state = build_ui_state(&snapshot, &JamConfig::default(), &shared);
        let grouped = state.by_participant();
        assert_eq!(grouped.len(), 2, "one account, two devices, two rows");
        assert_eq!(grouped[0].1[0].stream_name, "Guitar");
        assert_eq!(grouped[1].1[0].stream_name, "Talkback");
        // And both are still the same account.
        assert_eq!(state.streams_for_user("usr_1").len(), 2);
    }

    #[test]
    fn an_invite_link_is_taken_apart_into_a_code_and_a_secret() {
        let link =
            parse_jam_link("https://jam.futureboard.studio/j/EWEFDN#s3cr3t").expect("parsed");
        assert_eq!(link.code, "EWEFDN");
        assert_eq!(link.secret.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn a_room_link_carries_no_secret_and_a_bare_code_is_still_a_room() {
        // No fragment: this is the shareable room link, which only lets in an
        // account the jam already admits. Inventing a secret for it would turn
        // a permission error into "invalid link".
        let link = parse_jam_link("https://jam.futureboard.studio/j/EWEFDN").expect("parsed");
        assert_eq!(link.code, "EWEFDN");
        assert!(link.secret.is_none());

        // And the shape people read out loud.
        assert_eq!(parse_jam_link("EWEFDN").expect("parsed").code, "EWEFDN");
    }

    #[test]
    fn the_shapes_a_link_arrives_in_all_name_the_same_room() {
        for raw in [
            "  https://jam.futureboard.studio/j/EWEFDN/  ",
            "<https://jam.futureboard.studio/j/EWEFDN>",
            "jam.futureboard.studio/j/EWEFDN",
            "https://jam.futureboard.studio/j/EWEFDN?from=chat",
        ] {
            assert_eq!(
                parse_jam_link(raw)
                    .unwrap_or_else(|| panic!("{raw:?} should parse"))
                    .code,
                "EWEFDN",
                "{raw:?}"
            );
        }
    }

    #[test]
    fn something_that_is_not_a_jam_link_is_refused_rather_than_guessed_at() {
        // A request for a room that was never named is worse than a refusal:
        // the user is told the link is fine and the join simply fails.
        assert!(parse_jam_link("").is_none());
        assert!(parse_jam_link("https://example.com/some/other/page").is_none());
        assert!(parse_jam_link("not a code").is_none());
        // An empty fragment is a link with no secret, not a secret of "".
        let link = parse_jam_link("https://jam.futureboard.studio/j/EWEFDN#").expect("parsed");
        assert!(link.secret.is_none());
    }

    #[test]
    fn channel_labels_fall_back_to_the_layout_convention() {
        let mut summary = stream("str_1", "pcp_1", "usr_1", "dev", "Guitar");
        summary.channel_metadata.clear();
        assert_eq!(channel_labels(&summary), vec!["L", "R"]);

        summary.channels = 1;
        summary.channel_metadata.clear();
        assert_eq!(channel_labels(&summary), vec!["Mono"]);
    }
}

#[cfg(test)]
mod send_label_tests {
    use super::{publish_key_labels, send_tap_label, JamSendTap};
    use DirectAudio::jam_bus::{
        PUBLISH_KEY_LIVE_INPUT, PUBLISH_KEY_MASTER, PUBLISH_KEY_MONITOR, PUBLISH_KEY_MULTITRACK,
    };

    /// Master and Monitor are different signals and must never read as the same
    /// thing: one is the mix an export gets, the other is what the engineer is
    /// actually hearing.
    #[test]
    fn master_and_monitor_are_never_labelled_the_same() {
        assert_ne!(
            send_tap_label(JamSendTap::Master),
            send_tap_label(JamSendTap::Monitor)
        );
        assert_ne!(
            publish_key_labels(PUBLISH_KEY_MASTER),
            publish_key_labels(PUBLISH_KEY_MONITOR)
        );
    }

    #[test]
    fn every_publish_key_has_words_a_person_can_read() {
        for key in [
            PUBLISH_KEY_MASTER,
            PUBLISH_KEY_MONITOR,
            PUBLISH_KEY_LIVE_INPUT,
            PUBLISH_KEY_MULTITRACK,
        ] {
            let (name, tap) = publish_key_labels(key);
            assert!(!name.is_empty() && !tap.is_empty(), "{key}");
            assert_ne!(name, key, "{key} is shown raw");
        }
        let (name, tap) = publish_key_labels("track:trk_7");
        assert_eq!(name, "Track trk_7");
        assert_eq!(tap, "Track");
    }
}

#[cfg(test)]
mod stream_identity_tests {
    use super::{monogram_for, stream_identity};

    /// A face belongs to a jam stream and to nothing else. A hardware device
    /// and the jam *send* device both have to fall through to an icon, or the
    /// port menu would put somebody's picture on an audio interface.
    #[test]
    fn only_a_jam_stream_has_a_person_behind_it() {
        assert!(stream_identity("Focusrite USB ASIO").is_none());
        assert!(stream_identity("jam-send").is_none());
        // A jam device id with nobody in the room resolves to nobody, rather
        // than to a half-built identity with an empty name.
        assert!(stream_identity("jam:str_gone").is_none());
    }

    /// The monogram stands in for a picture that has not landed — or will never
    /// land — so it has to be right for every shape of name the room can carry.
    #[test]
    fn a_monogram_is_at_most_two_characters_of_whoever_it_is() {
        assert_eq!(monogram_for("Aria Kim"), "AK");
        assert_eq!(monogram_for("Aria Mae Kim"), "AM");
        assert_eq!(monogram_for("@nut"), "NU");
        assert_eq!(monogram_for("nut"), "NU");
        assert_eq!(monogram_for("x"), "X");
        assert_eq!(monogram_for("  "), "");
        for name in ["Aria Kim", "@nut", "n", ""] {
            assert!(monogram_for(name).chars().count() <= 2, "{name}");
        }
    }
}
