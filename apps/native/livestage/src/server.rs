//! LiveStage without a window.
//!
//! Runs a session on the interface and takes commands two ways: as JSON lines
//! on stdin, answered with one JSON line each on stdout, and from the web UI
//! — a page the server serves itself, talking over a WebSocket. Meant for a
//! rack machine, a Raspberry Pi under the stage, or a Linux server with no
//! desktop — there it is built with `--no-default-features` and runs the
//! built-in effects only.
//!
//! ```txt
//! livestage-server --list-devices
//! livestage-server --session show.json --output "hw:CARD=USB" --rate 48000
//! livestage-server --session show.json --http 0.0.0.0:8730   # web UI for the LAN
//! livestage-server --session show.json --daemon        # no stdin; stop with SIGTERM
//! ```
//!
//! Commands are the engine's ([`livestage_engine::Command`]) plus a few for
//! the server itself:
//!
//! ```txt
//! {"cmd":"set_fader","strip":{"kind":"channel","id":1},"db":-6}
//! {"cmd":"start_recording"}
//! {"cmd":"status"}   {"cmd":"meters"}   {"cmd":"session"}   {"cmd":"effects"}
//! {"cmd":"devices"}  {"cmd":"inserts"}  {"cmd":"installed"}
//! {"cmd":"save"}     {"cmd":"save","path":"other.json"}     {"cmd":"quit"}
//! {"cmd":"library"}  {"cmd":"library_save","name":"Lead vox","category":"Vocal","settings":{…},"sections":["processing"]}
//! {"cmd":"library_apply","item":"factory-vocal","targets":[{"kind":"channel","id":3}]}
//! {"cmd":"library_rename","item":"…","name":"…","category":"…"}   {"cmd":"library_delete","item":"…"}
//! ```
//!
//! A web client sends the same commands, tagged with a number echoed in its
//! reply — as `"id"`, or as `"req"` on a command whose own `"id"` is its
//! subject (a scene's, a disk's) — and is pushed the session (scene bodies
//! left out), the undo history, the current scene and whether it was changed
//! since, the strip library, each save, the status and the meters as they
//! change. It cannot quit the server, save anywhere but the session file,
//! move the recording folder, or load a plug-in the catalog does not list.
//!
//! On the LiveStage appliance the recording folder follows the disk picked on
//! the web UI's Setup page — `{"cmd":"storage","op":"use"|"eject"|"format",…}`,
//! asked of the appliance's storage service ([`storage`]) — and clients are
//! pushed a `storage` message with the disks whenever they change.
//!
//! The session file is saved atomically every `--autosave-seconds` (30 by
//! default) when something changed, right after a scene is stored, and on the
//! way out. The strip library ([`library`]) is its own file, kept across
//! shows.

#[cfg(test)]
mod access_tests;
// Shared with the desktop app; the readout formatting is the GUI's.
#[allow(dead_code)]
mod fader_law;
mod library;
mod midi;
mod osc;
mod remote;
mod storage;
mod storage_api;
mod users;
mod web;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, SyncSender};
use std::time::{Duration, Instant, SystemTime};

use livestage_engine::{
    Command, Id, InsertPlugin, InsertState, LiveEngine, RecallScope, Section, Session, StripRef,
    StripSettings,
};
use serde::Serialize;
use serde_json::{Value, json};
use web::{ClientId, Inbound};

static STOP: AtomicBool = AtomicBool::new(false);

/// Where the web UI listens unless told otherwise: this machine only.
const DEFAULT_HTTP: &str = "127.0.0.1:8730";
/// How often web clients get meters, the session (when it changed) and the
/// status.
const METER_PERIOD: Duration = Duration::from_millis(33);
const SESSION_PERIOD: Duration = Duration::from_millis(80);
const STATUS_PERIOD: Duration = Duration::from_millis(250);
/// How long a save waits for third-party plug-ins to hand over their state.
const SAVE_TIMEOUT: Duration = Duration::from_secs(2);
/// The periodic autosave unless `--autosave-seconds` says otherwise.
const DEFAULT_AUTOSAVE_SECONDS: u64 = 30;

struct Options {
    session: Option<PathBuf>,
    host: Option<String>,
    input: Option<String>,
    output: Option<String>,
    rate: Option<u32>,
    buffer: Option<u32>,
    record_dir: Option<PathBuf>,
    http: Option<SocketAddr>,
    list_devices: bool,
    daemon: bool,
    /// Save without being asked: on the way out, after a scene is stored,
    /// and every `autosave_seconds`. `--no-autosave` turns all of it off.
    autosave: bool,
    /// The periodic autosave's period; 0 = off.
    autosave_seconds: u64,
    /// The strip library's file; `None`: next to the session file.
    library: Option<PathBuf>,
    /// The users file; `None`: next to the session file.
    users: Option<PathBuf>,
    /// The remote (OSC/MIDI) settings file; `None`: next to the session file.
    remote: Option<PathBuf>,
}

const USAGE: &str = "usage: livestage-server [--session FILE] [--host NAME] [--input DEVICE] \
[--output DEVICE] [--rate HZ] [--buffer FRAMES] [--record-dir DIR] [--http ADDR:PORT | --no-http] \
[--library FILE] [--users FILE] [--remote FILE] [--autosave-seconds N] [--daemon] [--no-autosave] \
[--list-devices]

  --session FILE        the show: loaded at start, saved atomically (FILE.bak keeps the one before)
  --library FILE        the strip library, kept across shows (default: library.json next to the
                        session file; with no session it lives in memory only)
  --users FILE          who may log in to the web UI and with which role (default: users.json next
                        to the session file; with no session, memory only). No users: anyone who
                        reaches the web UI has full control
  --remote FILE         the OSC and MIDI remote settings (default: remote.json next to the session
                        file; with no session, memory only). OSC listens on the --http address
  --autosave-seconds N  save the session every N seconds when it changed (default 30, 0 = off);
                        it is also saved right after a scene is stored, and on the way out
  --no-autosave         never save the session without being asked
  --http ADDR:PORT      the web UI (default 127.0.0.1:8730); --no-http for none
  --daemon              no stdin; stop with SIGINT or SIGTERM";

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        session: None,
        host: None,
        input: None,
        output: None,
        rate: None,
        buffer: None,
        record_dir: None,
        http: DEFAULT_HTTP.parse().ok(),
        list_devices: false,
        daemon: false,
        autosave: true,
        autosave_seconds: DEFAULT_AUTOSAVE_SECONDS,
        library: None,
        users: None,
        remote: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--session" => options.session = Some(PathBuf::from(value("--session")?)),
            "--host" => options.host = Some(value("--host")?),
            "--input" => options.input = Some(value("--input")?),
            "--output" => options.output = Some(value("--output")?),
            "--rate" => {
                options.rate = Some(
                    value("--rate")?
                        .parse()
                        .map_err(|_| "--rate needs a number")?,
                )
            }
            "--buffer" => {
                options.buffer = Some(
                    value("--buffer")?
                        .parse()
                        .map_err(|_| "--buffer needs a number")?,
                )
            }
            "--record-dir" => options.record_dir = Some(PathBuf::from(value("--record-dir")?)),
            "--http" => {
                options.http = Some(
                    value("--http")?
                        .parse()
                        .map_err(|_| "--http needs an address like 0.0.0.0:8730")?,
                )
            }
            "--no-http" => options.http = None,
            "--list-devices" => options.list_devices = true,
            "--daemon" => options.daemon = true,
            "--no-autosave" => options.autosave = false,
            "--autosave-seconds" => {
                options.autosave_seconds = value("--autosave-seconds")?
                    .parse()
                    .map_err(|_| "--autosave-seconds needs a whole number of seconds")?
            }
            "--library" => options.library = Some(PathBuf::from(value("--library")?)),
            "--users" => options.users = Some(PathBuf::from(value("--users")?)),
            "--remote" => options.remote = Some(PathBuf::from(value("--remote")?)),
            "-h" | "--help" => return Err(USAGE.to_string()),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(options)
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    if options.list_devices {
        println!(
            "{}",
            serde_json::to_string_pretty(&devices(options.host.as_deref())).unwrap_or_default()
        );
        return;
    }

    let mut session = match &options.session {
        Some(path) if path.exists() => match Session::load(path) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("[livestage] {error}");
                std::process::exit(1);
            }
        },
        _ => Session::default(),
    };
    let fresh = options.session.as_ref().is_none_or(|path| !path.exists());
    if let Some(host) = &options.host {
        session.audio.host = Some(host.clone());
    }
    if let Some(input) = &options.input {
        session.audio.input_device = Some(input.clone());
    }
    if let Some(output) = &options.output {
        session.audio.output_device = Some(output.clone());
    }
    if let Some(rate) = options.rate {
        session.audio.sample_rate = rate;
    }
    if let Some(buffer) = options.buffer {
        session.audio.buffer_frames = buffer;
    }
    if let Some(dir) = &options.record_dir {
        session.recording.folder = Some(dir.clone());
    }

    let mut engine = LiveEngine::new(session);
    if fresh {
        // No file yet: one channel per interface input, faders down.
        let inputs = engine.input_channels();
        for index in 0..inputs {
            let _ = engine.apply(Command::AddChannel {
                name: format!("Input {}", index + 1),
                input: livestage_engine::InputPatch::mono(index as u16),
            });
        }
    }
    let status = engine.status();
    eprintln!(
        "[livestage] {} - {} Hz, {} in / {} out{}",
        status.output_device.as_deref().unwrap_or("no device"),
        status.sample_rate,
        status.in_channels,
        status.out_channels,
        status
            .error
            .as_deref()
            .map(|e| format!(" - {e}"))
            .unwrap_or_default()
    );

    let (library, log) = library::Library::open(library_path(&options));
    for line in log {
        eprintln!("[livestage] {line}");
    }
    if library.path().is_none() {
        eprintln!(
            "[livestage] no session file: the strip library lives in memory only and is lost on \
             exit (start with --session FILE or --library FILE to keep it)"
        );
    }
    // A users file that cannot be read stops the server: going on in open
    // mode would hand the mixer to anyone on the network.
    let users = match users::Users::open(beside_session(&options, &options.users, "users.json")) {
        Ok(users) => users,
        Err(error) => {
            eprintln!("[livestage] {error} (fix it, or remove it to let every web client in)");
            std::process::exit(1);
        }
    };
    if users.open_mode() {
        eprintln!("[livestage] no users: every web client has full control");
    } else {
        eprintln!(
            "[livestage] {} users: the web UI asks for a name and PIN",
            users.count()
        );
    }
    // A session read from its file is what the file holds; a new one is
    // not saved yet.
    let saves = Saves::new(
        autosave_period(&options),
        Instant::now(),
        (!fresh).then(|| engine.revision()),
    );

    install_stop_handler();
    // Kept for the whole run, so the queue never reads as closed: stdin's
    // end arrives as its own message.
    let (inbound_tx, inbound) = std::sync::mpsc::channel::<Inbound>();
    if !options.daemon {
        spawn_stdin_reader(inbound_tx.clone());
    }
    if let Some(addr) = options.http {
        match web::serve(addr, inbound_tx.clone()) {
            Ok(bound) => {
                eprintln!("[livestage] web UI on http://{bound}/");
                if !bound.ip().is_loopback() {
                    eprintln!(
                        "[livestage] the web UI is open to the network: anyone who can reach \
                         {bound} controls this mixer"
                    );
                }
                if !web::has_assets() {
                    eprintln!("[livestage] (this build has no web UI page; /ws still works)");
                }
            }
            Err(error) => eprintln!("[livestage] web UI not started on {addr}: {error}"),
        }
    }

    let storage = storage::Storage::start({
        let inbound = inbound_tx.clone();
        Box::new(move |done| {
            let _ = inbound.send(Inbound::Storage(done));
        })
    });
    // OSC listens where the web UI does (127.0.0.1: this machine only).
    let remote_ip = options
        .http
        .map_or(std::net::IpAddr::from([127, 0, 0, 1]), |addr| addr.ip());
    let (remote, log) = remote::Remote::open(
        beside_session(&options, &options.remote, "remote.json"),
        remote_ip,
        // The last sender handed out: the remote keeps it for the whole run.
        Some(remote::RemoteSender::new(inbound_tx)),
    );
    for line in log {
        eprintln!("[livestage] {line}");
    }
    let mut server = Server {
        options,
        clients: Vec::new(),
        catalog: Vec::new(),
        sent_revision: None,
        sent_history: None,
        scene_light: SceneLight::default(),
        watches: HashMap::new(),
        watched: HashSet::new(),
        storage,
        library,
        saves,
        users,
        remote,
    };
    let stdout = std::io::stdout();
    let start = Instant::now();
    let (mut next_meters, mut next_session, mut next_status) = (start, start, start);
    // Said once, when the device has first called back.
    let mut realtime_told = false;
    loop {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        engine.poll();
        if !realtime_told {
            if let Some(realtime) = engine.status().realtime {
                realtime_told = true;
                if realtime {
                    eprintln!("[livestage] audio threads at real-time priority");
                } else {
                    eprintln!(
                        "[livestage] audio threads at normal priority (real-time refused: no \
                         RLIMIT_RTPRIO or CAP_SYS_NICE); other load can make the audio drop out"
                    );
                }
            }
        }
        // A take that just ended may have kept the recorder off the disk
        // the storage service now records to.
        server.follow_storage(&mut engine);

        let now = Instant::now();
        if server.saves.due(now, engine.revision()) {
            server.autosave(&mut engine);
        }
        if !server.clients.is_empty() {
            if now >= next_meters {
                next_meters = now + METER_PERIOD;
                server.broadcast(&meters_message(&engine));
                server.push_telemetry(&engine);
            }
            if now >= next_session {
                next_session = now + SESSION_PERIOD;
                server.push_session_if_changed(&engine);
                server.push_history_if_changed(&engine);
            }
            if now >= next_status {
                next_status = now + STATUS_PERIOD;
                server.broadcast(&status_message(&engine, &server.saves));
                server.push_scene_state_if_changed(&engine);
                if server.users.expire(now) {
                    server.refresh_auth(&engine);
                }
            }
        }
        // OSC subscriptions, MIDI ports coming and going, feedback.
        if server.remote.active() {
            let live = remote::Live::of(&engine.status());
            let messages = server
                .remote
                .tick(engine.session(), engine.revision(), live, now);
            for message in messages {
                server.send_to_admins(&message);
            }
        }
        let wake = if server.clients.is_empty() {
            now + Duration::from_millis(50)
        } else {
            next_meters.min(next_session).min(next_status)
        };

        match inbound.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(Inbound::Stdin(line)) => {
                let (reply, quit) = match server.storage_command(&engine, &line, None) {
                    Some(Ok(())) => continue, // answered when the service is done
                    Some(Err(reply)) => (reply, false),
                    None => handle_line(&mut engine, &mut server, &line, Source::Stdin),
                };
                print_reply(&stdout, &reply);
                if quit {
                    break;
                }
            }
            Ok(Inbound::StdinClosed) => break,
            Ok(Inbound::Web(id, text)) => {
                let request = serde_json::from_str::<Value>(&text).ok();
                let request_id = request.as_ref().and_then(request_tag);
                // Logging in and out, the lock, and who may do what — before
                // anything else looks at the command.
                if let Some((reply, changed)) = server.gate(id, request.as_ref()) {
                    server.send_to(id, &web_reply(reply, request_id));
                    if changed {
                        server.refresh_auth(&engine);
                    }
                    continue;
                }
                let reply = match server.storage_command(&engine, &text, Some(id)) {
                    Some(Ok(())) => continue,
                    Some(Err(reply)) => reply,
                    None => handle_line(&mut engine, &mut server, &text, Source::Web(id)).0,
                };
                server.send_to(id, &web_reply(reply, request_id));
            }
            Ok(Inbound::Storage(done)) => {
                if let Some((origin, reply)) = server.storage.take(done) {
                    match origin.client {
                        Some(client) => server.send_to(client, &web_reply(reply, origin.request)),
                        None => print_reply(&stdout, &reply),
                    }
                }
                server.follow_storage(&mut engine);
                server.push_session_if_changed(&engine);
                server.push_storage_if_changed(&engine);
            }
            Ok(Inbound::Joined(id, outbox, peer)) => {
                // Bring the others up to now first, so that what the new
                // client is sent is what everyone has.
                server.push_session_if_changed(&engine);
                server.push_history_if_changed(&engine);
                server.push_scene_state_if_changed(&engine);
                server.push_storage_if_changed(&engine);
                // Its `auth`, then — when it needs no log-in — the rest.
                server.clients.push((id, outbox));
                server.users.join(id, peer);
                server.refresh_auth(&engine);
            }
            Ok(Inbound::Left(id)) => {
                server.clients.retain(|(client, _)| *client != id);
                server.watches.remove(&id);
                server.users.leave(id);
            }
            Ok(Inbound::Remote(event)) => remote_event(&mut engine, &mut server, event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    server.clients.clear();
    if let Some(summary) = engine.stop_recording() {
        eprintln!(
            "[livestage] recording stopped: {} file(s) in {}",
            summary.files.len(),
            summary.folder.display()
        );
    }
    if server.options.autosave {
        if let Some(path) = server.options.session.clone() {
            match server.save_session(&mut engine, &path, true) {
                Ok(()) => eprintln!("[livestage] session saved to {}", path.display()),
                Err(error) => eprintln!("[livestage] session not saved: {error}"),
            }
        }
    }
}

/// The library file `--library` names, else `library.json` next to the
/// session file; `None` (in memory only) with neither.
fn library_path(options: &Options) -> Option<PathBuf> {
    options.library.clone().or_else(|| {
        options
            .session
            .as_ref()
            .map(|session| session.with_file_name("library.json"))
    })
}

/// `explicit` (from `--users`, `--remote`), else `name` next to the session
/// file; `None` (in memory only) with neither.
fn beside_session(options: &Options, explicit: &Option<PathBuf>, name: &str) -> Option<PathBuf> {
    explicit.clone().or_else(|| {
        options
            .session
            .as_ref()
            .map(|session| session.with_file_name(name))
    })
}

/// The periodic autosave's period: off with `--no-autosave`,
/// `--autosave-seconds 0` or no session file to save to.
fn autosave_period(options: &Options) -> Option<Duration> {
    (options.autosave && options.autosave_seconds > 0 && options.session.is_some())
        .then(|| Duration::from_secs(options.autosave_seconds))
}

/// When the session file was last written, and when the next periodic
/// autosave is due. Time comes in as `now`, so tests drive the clock.
struct Saves {
    period: Option<Duration>,
    next_due: Instant,
    /// The engine's revision the session file holds; `None`: never saved.
    saved_revision: Option<u64>,
    /// `{"path","at","auto"}` of the last save, for the status.
    last: Option<Value>,
}

impl Saves {
    fn new(period: Option<Duration>, now: Instant, saved_revision: Option<u64>) -> Self {
        Self {
            period,
            next_due: now + period.unwrap_or_default(),
            saved_revision,
            last: None,
        }
    }

    /// Whether to autosave now: a period has passed since the last look and
    /// the session moved since it was last saved. Each call past the due
    /// time starts the next period, so a failed save is tried once a period,
    /// not on every loop.
    fn due(&mut self, now: Instant, revision: u64) -> bool {
        let Some(period) = self.period else {
            return false;
        };
        if now < self.next_due {
            return false;
        }
        self.next_due = now + period;
        self.unsaved(revision)
    }

    /// The session file now holds `revision`.
    fn saved(&mut self, revision: u64, last: Value) {
        self.saved_revision = Some(revision);
        self.last = Some(last);
    }

    fn unsaved(&self, revision: u64) -> bool {
        self.saved_revision != Some(revision)
    }
}

/// `{"type":"saved","path":…,"at":"2026-10-09T21:04:05Z","auto":…}`.
fn saved_message(path: &Path, at: SystemTime, auto: bool) -> Value {
    json!({"type": "saved", "path": path, "at": rfc3339(at), "auto": auto})
}

/// `at` as an RFC 3339 UTC timestamp, to the second.
fn rfc3339(at: SystemTime) -> String {
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest / 60 % 60,
        rest % 60
    )
}

/// A reply as a web client gets it: tagged, with its request's id.
/// It carries the tag as `id` and as `req` (see [`request_tag`]).
fn web_reply(mut reply: Value, request_id: Option<Value>) -> Value {
    if let Value::Object(fields) = &mut reply {
        fields.insert("type".to_string(), json!("reply"));
        if let Some(request_id) = request_id {
            fields.insert("id".to_string(), request_id.clone());
            fields.insert("req".to_string(), request_id);
        }
    }
    reply
}

/// What a web client tagged its request with, for the reply to carry back:
/// `req` when given — on a command whose own `id` is its subject (a scene's,
/// a disk's) — else `id`.
fn request_tag(request: &Value) -> Option<Value> {
    request.get("req").or_else(|| request.get("id")).cloned()
}

fn print_reply(stdout: &std::io::Stdout, reply: &Value) {
    let mut out = stdout.lock();
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
}

fn spawn_stdin_reader(inbound: Sender<Inbound>) {
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            if inbound.send(Inbound::Stdin(line)).is_err() {
                return;
            }
        }
        let _ = inbound.send(Inbound::StdinClosed);
    });
}

/// Who sent a command. stdin is whoever started the server; a web client is
/// anyone who can reach the port; the remote is an OSC packet or a mapped
/// MIDI control, acting as an engineer. Only stdin is trusted as it is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Stdin,
    Web(ClientId),
    Remote,
}

struct Server {
    options: Options,
    clients: Vec<(ClientId, SyncSender<String>)>,
    /// The inserts each client has an editor open on.
    watches: HashMap<ClientId, HashSet<Id>>,
    /// The inserts the engine was last told to measure: every client's.
    watched: HashSet<Id>,
    /// Studio's plug-in catalog, read when first asked for: a web client
    /// may only load what is in it.
    #[cfg_attr(not(feature = "external-plugins"), allow(dead_code))]
    catalog: Vec<Value>,
    /// The engine's revision of the session the web clients last saw.
    sent_revision: Option<u64>,
    /// The undo history the web clients last saw.
    sent_history: Option<Value>,
    /// The current scene and its "modified" light.
    scene_light: SceneLight,
    /// The appliance's disks and where recordings go.
    storage: storage::Storage,
    /// The strip library.
    library: library::Library,
    /// The session file's saves and the periodic autosave.
    saves: Saves,
    /// Who is logged in where, and who may do what.
    users: users::Users,
    /// OSC and MIDI.
    remote: remote::Remote,
}

/// What the server needs of the engine beyond the commands it relays:
/// [`LiveEngine`], or a stand-in in the tests.
trait Mixer {
    fn session(&self) -> &Session;
    /// Bumps on every change to the session.
    fn revision(&self) -> u64;
    fn history(&self) -> livestage_engine::History;
    fn scene_differs(&self, id: Id) -> Option<bool>;
    fn apply_with_note(&mut self, command: Command) -> Result<Option<String>, String>;
    /// An atomic write of the session (plug-in state captured first).
    fn save(&mut self, path: &Path, timeout: Duration) -> Result<(), String>;
}

impl Mixer for LiveEngine {
    fn session(&self) -> &Session {
        LiveEngine::session(self)
    }
    fn revision(&self) -> u64 {
        LiveEngine::revision(self)
    }
    fn history(&self) -> livestage_engine::History {
        LiveEngine::history(self)
    }
    fn scene_differs(&self, id: Id) -> Option<bool> {
        LiveEngine::scene_differs(self, id)
    }
    fn apply_with_note(&mut self, command: Command) -> Result<Option<String>, String> {
        LiveEngine::apply_with_note(self, command)
    }
    fn save(&mut self, path: &Path, timeout: Duration) -> Result<(), String> {
        LiveEngine::save(self, path, timeout)
    }
}

/// The current scene and whether the mix has moved from it, as last worked
/// out and as last sent.
#[derive(Default)]
struct SceneLight {
    /// What `modified` was worked out from: (revision, current scene).
    checked: Option<(u64, Option<Id>)>,
    modified: bool,
    sent: Option<(Option<Id>, bool)>,
}

impl SceneLight {
    /// `{"type":"scene_state",…}` when the current scene or its light
    /// changed since last sent. `differs` (the engine's `scene_differs`) is
    /// asked only when the session or the current scene moved.
    fn message_if_changed(
        &mut self,
        revision: u64,
        current: Option<Id>,
        differs: impl FnOnce(Id) -> Option<bool>,
    ) -> Option<Value> {
        if self.checked != Some((revision, current)) {
            self.modified = current.and_then(differs).unwrap_or(false);
            self.checked = Some((revision, current));
        }
        let state = (current, self.modified);
        if self.sent == Some(state) {
            return None;
        }
        self.sent = Some(state);
        Some(scene_state_message(current, self.modified))
    }
}

fn scene_state_message(current: Option<Id>, modified: bool) -> Value {
    json!({"type": "scene_state", "current": current, "modified": modified})
}

impl Server {
    /// Queue `message` for every client let in (one that has to log in first
    /// is sent nothing but `auth`). One whose outbox is full has stopped
    /// reading: it is dropped, and reconnects to a fresh snapshot.
    fn broadcast(&mut self, message: &Value) {
        let text = message.to_string();
        let users = &self.users;
        self.clients.retain(|(client, outbox)| {
            !users.admitted(*client) || outbox.try_send(text.clone()).is_ok()
        });
    }

    /// Queue `message` for the clients logged in as an admin (in open mode:
    /// every client).
    fn send_to_admins(&mut self, message: &Value) {
        let text = message.to_string();
        let users = &self.users;
        self.clients.retain(|(client, outbox)| {
            let admin = users
                .principal(*client)
                .is_some_and(|p| p.role == users::Role::Admin);
            !admin || outbox.try_send(text.clone()).is_ok()
        });
    }

    /// Answer the `auth` family (`login`, `lock`, `user_add`, …) and check
    /// every other command from web client `id` against its rights: `Some`
    /// is the reply (and whether some client's `auth` changed — call
    /// [`refresh_auth`](Self::refresh_auth) after sending it); `None` lets
    /// the command through.
    fn gate(&mut self, id: ClientId, request: Option<&Value>) -> Option<(Value, bool)> {
        // Unreadable: the command path answers with the parse error.
        let request = request?;
        let cmd = request
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let now = Instant::now();
        if let Some(outcome) = self.users.command(Some(id), cmd, request, now) {
            return Some((outcome.reply, outcome.changed));
        }
        if let Err(refusal) = self.users.check(id, cmd, request) {
            return Some((refusal, false));
        }
        self.users.touch(id, now);
        None
    }

    /// Send each client its `auth` when it changed; one just let in gets
    /// everything a page needs, one just shut out stops getting telemetry.
    fn refresh_auth(&mut self, engine: &LiveEngine) {
        self.refresh_auth_with(|server, outbox| server.welcome(engine, outbox));
    }

    fn refresh_auth_with(&mut self, welcome: impl Fn(&Self, &SyncSender<String>)) {
        for refresh in self.users.refresh() {
            let Some(outbox) = self
                .clients
                .iter()
                .find(|(client, _)| *client == refresh.client)
                .map(|(_, outbox)| outbox.clone())
            else {
                continue;
            };
            if let Some(auth) = &refresh.auth {
                let _ = outbox.try_send(auth.to_string());
            }
            match refresh.admitted {
                Some(true) => welcome(self, &outbox),
                Some(false) => {
                    self.watches.remove(&refresh.client);
                }
                None => {}
            }
        }
    }

    fn send_to(&mut self, id: ClientId, message: &Value) {
        let text = message.to_string();
        self.clients
            .retain(|(client, outbox)| *client != id || outbox.try_send(text.clone()).is_ok());
    }

    /// Telemetry for every insert an editor is open on, to the clients that
    /// have it open. The engine measures only those, and is told again each
    /// time, so an insert rebuilt since keeps measuring.
    fn push_telemetry(&mut self, engine: &LiveEngine) {
        let wanted: HashSet<Id> = self.watches.values().flatten().copied().collect();
        for insert in self.watched.difference(&wanted) {
            engine.watch_insert(*insert, false);
        }
        for insert in &wanted {
            engine.watch_insert(*insert, true);
        }
        self.watched = wanted;
        let mut dropped = Vec::new();
        for insert in &self.watched {
            let Some(frame) = engine.insert_telemetry(*insert) else {
                continue;
            };
            if frame == livestage_engine::telemetry::TelemetryFrame::default() {
                continue;
            }
            let text = json!({"type": "telemetry", "insert": insert, "frame": frame}).to_string();
            for (client, outbox) in &self.clients {
                let watching = self
                    .watches
                    .get(client)
                    .is_some_and(|inserts| inserts.contains(insert))
                    && self.users.admitted(*client);
                if watching && outbox.try_send(text.clone()).is_err() {
                    dropped.push(*client);
                }
            }
        }
        for client in dropped {
            self.clients.retain(|(id, _)| *id != client);
            self.watches.remove(&client);
        }
    }

    /// The session, when the engine's revision moved since it was last sent
    /// — never compared or cloned whole: a show's scenes make it large.
    fn push_session_if_changed(&mut self, engine: &impl Mixer) {
        let revision = engine.revision();
        if self.sent_revision == Some(revision) {
            return;
        }
        self.sent_revision = Some(revision);
        self.broadcast(&session_message(engine.session()));
        // The storage message carries the folder the recorder writes to.
        self.push_storage_if_changed(engine);
    }

    fn push_history_if_changed(&mut self, engine: &impl Mixer) {
        let message = history_message(engine);
        if self.sent_history.as_ref() == Some(&message) {
            return;
        }
        self.broadcast(&message);
        self.sent_history = Some(message);
    }

    fn push_scene_state_if_changed(&mut self, engine: &impl Mixer) {
        let current = engine.session().current_scene;
        if let Some(message) =
            self.scene_light
                .message_if_changed(engine.revision(), current, |id| engine.scene_differs(id))
        {
            self.broadcast(&message);
        }
    }

    /// Write the session to `path`; when that is the session file, it now
    /// holds this revision. Every client hears of it.
    fn save_session(
        &mut self,
        engine: &mut impl Mixer,
        path: &Path,
        auto: bool,
    ) -> Result<(), String> {
        engine.save(path, SAVE_TIMEOUT)?;
        let message = saved_message(path, SystemTime::now(), auto);
        if self.options.session.as_deref() == Some(path) {
            // After the save: capturing plug-in state may itself have
            // moved the revision, and the file holds that.
            self.saves.saved(engine.revision(), saved_status(&message));
        }
        self.broadcast(&message);
        Ok(())
    }

    /// The periodic (or after-a-scene-store) save of the session file.
    fn autosave(&mut self, engine: &mut impl Mixer) {
        if !self.options.autosave {
            return;
        }
        let Some(path) = self.options.session.clone() else {
            return;
        };
        if let Err(error) = self.save_session(engine, &path, true) {
            eprintln!("[livestage] autosave failed: {error}");
        }
    }

    /// `{"cmd":"storage",…}`: `None` when `line` is something else; else
    /// `Ok` when it went to the storage service (its answer comes later), or
    /// the refusal to send back now.
    fn storage_command(
        &mut self,
        engine: &LiveEngine,
        line: &str,
        client: Option<ClientId>,
    ) -> Option<Result<(), Value>> {
        let value = serde_json::from_str::<Value>(line).ok()?;
        if value.get("cmd").and_then(Value::as_str) != Some("storage") {
            return None;
        }
        let origin = storage::Origin {
            client,
            request: request_tag(&value),
        };
        let asked = self
            .storage
            .ask(&value, engine.is_recording(), origin)
            .map_err(|error| json!({"ok": false, "error": error}));
        // Everyone sees the change under way.
        self.push_storage_if_changed(engine);
        Some(asked)
    }

    /// Point the recorder at the storage service's `recordings_dir` when it
    /// moved (another disk chosen, the chosen one gone or back), between
    /// takes only.
    fn follow_storage(&mut self, engine: &mut LiveEngine) {
        let current = engine.session().recording.folder.as_deref();
        let Some(folder) = self.storage.folder_to_apply(current, engine.is_recording()) else {
            return;
        };
        let mut settings = engine.session().recording.clone();
        settings.folder = Some(folder.clone());
        match engine.apply(Command::SetRecordSettings { settings }) {
            Ok(()) => eprintln!("[livestage] recording to {}", folder.display()),
            Err(error) => eprintln!(
                "[livestage] the recording folder stays: {error} ({} wanted)",
                folder.display()
            ),
        }
    }

    fn push_storage_if_changed(&mut self, engine: &impl Mixer) {
        let folder = engine.session().recording.resolved_folder();
        if let Some(message) = self.storage.message_if_changed(&folder) {
            self.broadcast(&message);
        }
    }

    /// Everything a page needs to draw itself.
    fn welcome(&self, engine: &LiveEngine, outbox: &SyncSender<String>) {
        // With each effect's wire table, defaults and factory presets: what
        // its editor is drawn from.
        let effects: Vec<Value> = livestage_engine::builtin_fx::BUILTIN_EFFECTS
            .iter()
            .map(|e| {
                json!({
                    "stem": e.stem,
                    "name": e.name,
                    "category": e.category,
                    "params": livestage_engine::builtin_fx::builtin_params(e.stem),
                    "spec": livestage_engine::builtin_fx::builtin_spec(e.stem),
                })
            })
            .collect();
        let hello = json!({
            "type": "hello",
            "version": env!("CARGO_PKG_VERSION"),
            "external_plugins": cfg!(feature = "external-plugins"),
            "session_path": self.options.session,
            "library_path": self.library.path(),
            "users_path": self.users.path(),
            "remote_path": self.remote.path(),
            "autosave_seconds": autosave_period(&self.options).map(|p| p.as_secs()),
            "effects": effects,
        });
        let storage = self
            .storage
            .message(&engine.session().recording.resolved_folder());
        let current = engine.session().current_scene;
        let modified = current
            .and_then(|id| engine.scene_differs(id))
            .unwrap_or(false);
        for message in [
            hello,
            session_message(engine.session()),
            status_message(engine, &self.saves),
            storage,
            history_message(engine),
            scene_state_message(current, modified),
            self.library.message(),
        ] {
            let _ = outbox.try_send(message.to_string());
        }
    }

    #[cfg(feature = "external-plugins")]
    fn refresh_catalog(&mut self) -> &[Value] {
        self.catalog = livestage_engine::external::installed_effects()
            .into_iter()
            .map(|e| {
                json!({
                    "name": e.name,
                    "vendor": e.vendor,
                    "format": e.format,
                    "path": e.path,
                    "class_id": e.class_id,
                })
            })
            .collect();
        &self.catalog
    }

    /// Whether the catalog lists this plug-in (read now if never read).
    #[cfg(feature = "external-plugins")]
    fn in_catalog(&mut self, path: &str, class_id: &str) -> bool {
        if self.catalog.is_empty() {
            self.refresh_catalog();
        }
        self.catalog
            .iter()
            .any(|e| e["path"] == json!(path) && e["class_id"] == json!(class_id))
    }
}

/// `{"type":"session","session":…}` with each scene as `{id,name,note,scope}`
/// — a scene's stored mix stays on the server.
fn session_message(session: &Session) -> Value {
    json!({"type": "session", "session": SessionView::of(session)})
}

/// The session as the web clients get it: every field of [`Session`] (a
/// test checks none is missing), the scenes without their mixes.
#[derive(Serialize)]
struct SessionView<'a> {
    name: &'a str,
    audio: &'a livestage_engine::AudioSettings,
    channels: &'a [livestage_engine::ChannelStrip],
    buses: &'a [livestage_engine::BusStrip],
    master: &'a livestage_engine::MasterStrip,
    matrices: &'a [livestage_engine::MatrixStrip],
    outputs: &'a [livestage_engine::OutputPatch],
    recording: &'a livestage_engine::RecordSettings,
    dcas: &'a [livestage_engine::Dca],
    mute_groups: &'a [livestage_engine::MuteGroup],
    monitor: &'a livestage_engine::MonitorSettings,
    talkback: &'a livestage_engine::TalkbackSettings,
    oscillator: &'a livestage_engine::OscillatorSettings,
    layers: &'a [livestage_engine::Layer],
    playback: &'a livestage_engine::PlaybackSettings,
    scenes: Vec<SceneSummary<'a>>,
    current_scene: Option<Id>,
    next_id: Id,
}

#[derive(Serialize)]
struct SceneSummary<'a> {
    id: Id,
    name: &'a str,
    note: &'a str,
    scope: &'a RecallScope,
}

impl<'a> SessionView<'a> {
    fn of(session: &'a Session) -> Self {
        let Session {
            name,
            audio,
            channels,
            buses,
            master,
            matrices,
            outputs,
            recording,
            dcas,
            mute_groups,
            monitor,
            talkback,
            oscillator,
            layers,
            playback,
            scenes,
            current_scene,
            next_id,
        } = session;
        Self {
            name,
            audio,
            channels,
            buses,
            master,
            matrices,
            outputs,
            recording,
            dcas,
            mute_groups,
            monitor,
            talkback,
            oscillator,
            layers,
            playback,
            scenes: scenes
                .iter()
                .map(|scene| SceneSummary {
                    id: scene.id,
                    name: &scene.name,
                    note: &scene.note,
                    scope: &scene.scope,
                })
                .collect(),
            current_scene: *current_scene,
            next_id: *next_id,
        }
    }
}

/// `{"type":"history","undo":"Fader Kick"|null,"redo":…,"undo_depth":n,"redo_depth":n}`.
fn history_message(engine: &impl Mixer) -> Value {
    let mut message = serde_json::to_value(engine.history()).unwrap_or_else(|_| json!({}));
    if let Value::Object(fields) = &mut message {
        fields.insert("type".to_string(), json!("history"));
    }
    message
}

/// The last save as the status carries it: `{"path","at","auto"}`.
fn saved_status(saved: &Value) -> Value {
    json!({"path": saved["path"], "at": saved["at"], "auto": saved["auto"]})
}

fn status_message(engine: &LiveEngine, saves: &Saves) -> Value {
    // Only inserts that are not simply running: a missing id is ready.
    let mut inserts = serde_json::Map::new();
    for (_, core) in engine.session().strip_cores() {
        for slot in &core.inserts {
            match engine.insert_state(slot.id) {
                InsertState::Ready => {}
                state => {
                    inserts.insert(slot.id.to_string(), insert_state_json(&state));
                }
            }
        }
    }
    json!({
        "type": "status",
        "status": engine.status(),
        "recording": engine.is_recording(),
        "inserts": inserts,
        "last_recording": engine.last_recording().map(summary_json),
        // When the session file was last written, and whether the mix has
        // moved since.
        "last_saved": saves.last,
        "unsaved": saves.unsaved(engine.revision()),
        // Also in `status.playback`; here for the transport display.
        "playback": engine.playback_status(),
    })
}

fn meters_message(engine: &LiveEngine) -> Value {
    let meters: Vec<Value> = engine
        .meters()
        .into_iter()
        .map(|(strip, levels)| json!({"strip": strip, "levels": levels}))
        .collect();
    let (left, right) = engine.monitor_meter();
    json!({
        "type": "meters",
        "meters": meters,
        "monitor": [left, right],
        "talkback": [engine.talkback_meter()],
    })
}

fn insert_state_json(state: &InsertState) -> Value {
    match state {
        InsertState::Ready => json!("ready"),
        InsertState::Loading => json!("loading"),
        InsertState::Failed(error) => json!({"failed": error}),
    }
}

fn summary_json(summary: &livestage_engine::recorder::RecordingSummary) -> Value {
    json!({
        "folder": summary.folder,
        "files": summary.files,
        "seconds": summary.seconds,
        "dropped_samples": summary.dropped_samples,
        "errors": summary.errors,
    })
}

fn handle_line(
    engine: &mut LiveEngine,
    server: &mut Server,
    line: &str,
    source: Source,
) -> (Value, bool) {
    match serde_json::from_str(line) {
        Ok(value) => handle(engine, server, value, source),
        Err(error) => (json!({"ok": false, "error": error.to_string()}), false),
    }
}

/// Answer one command. Returns the reply and whether to quit.
fn handle(
    engine: &mut LiveEngine,
    server: &mut Server,
    value: Value,
    source: Source,
) -> (Value, bool) {
    // Anyone but whoever started the server.
    let web = source != Source::Stdin;
    let reply = match value.get("cmd").and_then(Value::as_str) {
        // An editor opened (or closed) on a built-in insert: measure it, and
        // send this client what it measures.
        Some("watch_insert") => match source {
            Source::Web(client) => {
                let insert = value.get("insert").and_then(Value::as_u64);
                let watch = value.get("watch").and_then(Value::as_bool).unwrap_or(true);
                match insert.and_then(|i| Id::try_from(i).ok()) {
                    Some(insert) => {
                        let set = server.watches.entry(client).or_default();
                        if watch {
                            set.insert(insert);
                        } else {
                            set.remove(&insert);
                        }
                        json!({"ok": true})
                    }
                    None => json!({"ok": false, "error": "watch_insert needs \"insert\""}),
                }
            }
            Source::Stdin | Source::Remote => {
                json!({"ok": false, "error": "watch_insert is for web clients"})
            }
        },
        // Users from the console (a web client's were answered at the gate).
        Some(cmd) if source == Source::Stdin && users::FAMILY.contains(&cmd) => {
            let cmd = cmd.to_string();
            match server.users.command(None, &cmd, &value, Instant::now()) {
                Some(outcome) => {
                    if outcome.changed {
                        server.refresh_auth(engine);
                    }
                    outcome.reply
                }
                None => json!({"ok": false, "error": format!("unknown command {cmd}")}),
            }
        }
        // The OSC/MIDI setup (admins only: checked at the gate).
        Some(cmd) if source != Source::Remote && remote::COMMANDS.contains(&cmd) => {
            let cmd = cmd.to_string();
            match server.remote.command(&cmd, &value) {
                Some((reply, tell)) => {
                    if tell {
                        let message = server.remote.message();
                        server.send_to_admins(&message);
                    }
                    reply
                }
                None => json!({"ok": false, "error": format!("unknown command {cmd}")}),
            }
        }
        Some("quit") if web => {
            json!({"ok": false, "error": "quit is only taken on the server's console"})
        }
        Some("quit") => return (json!({"ok": true}), true),
        Some("status") => json!({"ok": true, "status": engine.status()}),
        Some("takes") => takes_reply(engine),
        Some("session") => json!({"ok": true, "session": engine.session()}),
        Some("meters") => {
            let mut message = meters_message(engine);
            json!({
                "ok": true,
                "meters": message["meters"].take(),
                "monitor": message["monitor"].take(),
                "talkback": message["talkback"].take(),
            })
        }
        Some("devices") => {
            // Another host's devices can be listed before switching to it.
            let host = match value.get("host").and_then(Value::as_str) {
                Some(host) => Some(host.to_string()),
                None => engine.session().audio.host.clone(),
            };
            json!({"ok": true, "devices": devices(host.as_deref())})
        }
        Some("effects") => {
            let effects: Vec<Value> = livestage_engine::builtin_fx::BUILTIN_EFFECTS
                .iter()
                .map(|e| {
                    json!({
                        "stem": e.stem,
                        "name": e.name,
                        "category": e.category,
                        "params": livestage_engine::builtin_fx::builtin_params(e.stem),
                    })
                })
                .collect();
            json!({"ok": true, "effects": effects})
        }
        #[cfg(feature = "external-plugins")]
        Some("installed") => {
            // The third-party effects Studio's scanner found on this machine.
            json!({"ok": true, "installed": server.refresh_catalog()})
        }
        #[cfg(not(feature = "external-plugins"))]
        Some("installed") => json!({"ok": true, "installed": []}),
        Some("inserts") => {
            // Every insert and whether it runs: a third-party one loads in
            // the plug-in host and can fail there.
            let engine: &LiveEngine = engine;
            let inserts: Vec<Value> = engine
                .session()
                .strip_cores()
                .flat_map(|(strip, core)| {
                    core.inserts.iter().map(move |slot| {
                        json!({
                            "strip": strip,
                            "id": slot.id,
                            "name": slot.plugin.display_name(),
                            "bypass": slot.bypass,
                            "state": insert_state_json(&engine.insert_state(slot.id)),
                        })
                    })
                })
                .collect();
            json!({"ok": true, "inserts": inserts})
        }
        Some("save") => {
            let asked = value.get("path").and_then(Value::as_str).map(PathBuf::from);
            if web && asked.is_some() {
                json!({"ok": false, "error": "the web UI saves to the session file only"})
            } else {
                match asked.or_else(|| server.options.session.clone()) {
                    Some(path) => match server.save_session(engine, &path, false) {
                        Ok(()) => json!({"ok": true, "path": path}),
                        Err(error) => json!({"ok": false, "error": error}),
                    },
                    None => json!({
                        "ok": false,
                        "error": "no session file: start the server with --session FILE"
                    }),
                }
            }
        }
        // Not onto a disk that is being switched, ejected or formatted.
        Some("start_recording") if server.storage.blocks_recording().is_some() => {
            json!({"ok": false, "error": server.storage.blocks_recording()})
        }
        Some("stop_recording") => match engine.stop_recording() {
            Some(summary) => {
                let mut reply = summary_json(&summary);
                reply["ok"] = json!(true);
                reply
            }
            None => json!({"ok": false, "error": "not recording"}),
        },
        Some(cmd) if cmd == "library" || cmd.starts_with("library_") => {
            let cmd = cmd.to_string();
            library_command(engine, server, &cmd, value, source)
        }
        _ => engine_command(engine, server, value, source),
    };
    (reply, false)
}

/// An OSC packet or a MIDI message: each command it comes to is checked
/// with an engineer's rights and run like a web client's.
fn remote_event(engine: &mut LiveEngine, server: &mut Server, event: remote::RemoteIn) {
    let live = remote::Live::of(&engine.status());
    let actions = server
        .remote
        .input(event, engine.session(), live, Instant::now());
    for command in actions.commands {
        let cmd = command
            .get("cmd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let ran = users::permit(&users::Principal::remote(), &cmd, &command).is_ok()
            && handle(engine, server, command, Source::Remote).0["ok"] == true;
        if !ran {
            server.remote.refused();
        }
    }
    for message in actions.messages {
        server.send_to_admins(&message);
    }
}

/// One of the engine's own commands, admitted and applied.
fn engine_command(
    engine: &mut impl Mixer,
    server: &mut Server,
    mut value: Value,
    source: Source,
) -> Value {
    // A request's tag is no part of the command. `id` stays: it is the
    // subject of the scene commands, and ignored by the others.
    if let Some(fields) = value.as_object_mut() {
        fields.remove("req");
    }
    match serde_json::from_value::<Command>(value) {
        Ok(command) => match admit(engine, server, command, source) {
            Ok(command) => run(engine, server, command),
            Err(error) => json!({"ok": false, "error": error}),
        },
        Err(error) => json!({"ok": false, "error": error.to_string()}),
    }
}

/// Apply an admitted engine command: `{"ok":true}`, with the engine's note
/// when it has one (what a recall left alone). A stored scene is saved to
/// the session file straight away.
fn run(engine: &mut impl Mixer, server: &mut Server, command: Command) -> Value {
    let stores_scene = matches!(command, Command::SceneStore { .. });
    match engine.apply_with_note(command) {
        Ok(note) => {
            if stores_scene {
                server.autosave(engine);
            }
            ok_reply(note)
        }
        Err(error) => json!({"ok": false, "error": error}),
    }
}

/// `{"ok":true}`, or `{"ok":true,"note":"…"}`.
fn ok_reply(note: Option<String>) -> Value {
    match note {
        Some(note) => json!({"ok": true, "note": note}),
        None => json!({"ok": true}),
    }
}

#[derive(serde::Deserialize)]
struct LibrarySave {
    name: String,
    #[serde(default)]
    category: Option<String>,
    settings: StripSettings,
    sections: Vec<Section>,
}

#[derive(serde::Deserialize)]
struct LibraryApply {
    item: String,
    targets: Vec<StripRef>,
}

#[derive(serde::Deserialize)]
struct LibraryRename {
    item: String,
    name: String,
    #[serde(default)]
    category: Option<String>,
}

#[derive(serde::Deserialize)]
struct LibraryItemRef {
    item: String,
}

/// `library`, `library_save`, `library_apply`, `library_delete`,
/// `library_rename`. A change is pushed to every client.
fn library_command(
    engine: &mut impl Mixer,
    server: &mut Server,
    cmd: &str,
    value: Value,
    source: Source,
) -> Value {
    fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }
    let before = server.library.revision();
    let result: Result<Value, String> = match cmd {
        "library" => Ok(json!({"ok": true, "items": server.library.message()["items"]})),
        "library_save" => parse::<LibrarySave>(value).and_then(|mut asked| {
            vet_settings(engine, server, &mut asked.settings, source)?;
            let id = server.library.save(
                &asked.name,
                asked.category.as_deref(),
                asked.settings,
                asked.sections,
            )?;
            Ok(json!({"ok": true, "item": id}))
        }),
        // The item's sections, pasted onto the targets: one engine command,
        // one undo step. The item is the server's own (vetted when saved).
        "library_apply" => parse::<LibraryApply>(value).and_then(|asked| {
            let command = server.library.paste_command(&asked.item, asked.targets)?;
            Ok(run(engine, server, command))
        }),
        "library_delete" => parse::<LibraryItemRef>(value).and_then(|asked| {
            server.library.delete(&asked.item)?;
            Ok(json!({"ok": true}))
        }),
        "library_rename" => parse::<LibraryRename>(value).and_then(|asked| {
            server
                .library
                .rename(&asked.item, &asked.name, asked.category.as_deref())?;
            Ok(json!({"ok": true}))
        }),
        other => Err(format!("unknown command {other}")),
    };
    if server.library.revision() != before {
        let message = server.library.message();
        server.broadcast(&message);
    }
    result.unwrap_or_else(|error| json!({"ok": false, "error": error}))
}

/// What a web client may not do with an engine command: load a plug-in
/// binary the catalog does not list (or hand it state of its own), or point
/// the recorder at another folder. stdin is trusted as it is.
fn admit(
    engine: &impl Mixer,
    server: &mut Server,
    mut command: Command,
    source: Source,
) -> Result<Command, String> {
    if source == Source::Stdin {
        return Ok(command);
    }
    match &mut command {
        Command::AddInsert {
            plugin:
                InsertPlugin::External {
                    path,
                    class_id,
                    state,
                    ..
                },
            ..
        } => {
            #[cfg(feature = "external-plugins")]
            if !server.in_catalog(path, class_id) {
                return Err(format!("{path} is not in the plug-in catalog"));
            }
            #[cfg(not(feature = "external-plugins"))]
            let _ = (path, class_id, server);
            *state = None;
        }
        Command::SetRecordSettings { settings } => {
            settings.folder = engine.session().recording.folder.clone();
        }
        Command::PlaybackLoad { folder } => *folder = listed_take(engine, folder)?,
        Command::PasteStrip { settings, .. } => vet_settings(engine, server, settings, source)?,
        _ => {}
    }
    Ok(command)
}

/// The inserts a web client pastes or keeps in the library: a third-party
/// plug-in must be in the catalog, and keeps its opaque state only when that
/// state is one an insert of the same plug-in in this session already has (a
/// copy of a strip) — never state of the client's own making.
fn vet_settings(
    engine: &impl Mixer,
    server: &mut Server,
    settings: &mut StripSettings,
    source: Source,
) -> Result<(), String> {
    if source == Source::Stdin {
        return Ok(());
    }
    let Some(inserts) = &mut settings.inserts else {
        return Ok(());
    };
    for insert in inserts {
        let InsertPlugin::External {
            path,
            class_id,
            state,
            ..
        } = &mut insert.plugin
        else {
            continue;
        };
        #[cfg(feature = "external-plugins")]
        if !server.in_catalog(path, class_id) {
            return Err(format!("{path} is not in the plug-in catalog"));
        }
        #[cfg(not(feature = "external-plugins"))]
        let _ = &server;
        if state.is_some() && !session_has_state(engine.session(), path, class_id, state) {
            *state = None;
        }
    }
    Ok(())
}

/// Whether an insert in `session` is plug-in `path`/`class_id` with `state`.
fn session_has_state(
    session: &Session,
    path: &str,
    class_id: &str,
    state: &Option<(String, String)>,
) -> bool {
    session
        .strip_cores()
        .flat_map(|(_, core)| &core.inserts)
        .any(|slot| {
            matches!(
                &slot.plugin,
                InsertPlugin::External { path: p, class_id: c, state: s, .. }
                    if p == path && c == class_id && s == state
            )
        })
}

/// `{"cmd":"takes"}`: the take folders in the recordings folder in use (the
/// storage target or `--record-dir`), newest first, with their files:
/// `{"ok":true,"folder":…,"takes":[{"name","path","files":[{"name","channels","rate","seconds"}]}]}`.
fn takes_reply(engine: &impl Mixer) -> Value {
    let root = engine.session().recording.resolved_folder();
    let takes = livestage_engine::playback::list_takes(&root);
    json!({"ok": true, "folder": root, "takes": takes})
}

/// The take a web client may load: one `takes` lists, named exactly — no
/// other path reaches the reader from the network.
fn listed_take(engine: &impl Mixer, asked: &Path) -> Result<PathBuf, String> {
    let root = engine.session().recording.resolved_folder();
    livestage_engine::playback::list_takes(&root)
        .into_iter()
        .map(|take| take.path)
        .find(|path| path == asked)
        .ok_or_else(|| {
            format!(
                "{} is not a take in the recordings folder ({})",
                asked.display(),
                root.display()
            )
        })
}

fn devices(host: Option<&str>) -> Value {
    let hosts = livestage_engine::device::host_names();
    let (inputs, outputs) = livestage_engine::device::list_devices(host);
    let describe = |devices: &[livestage_engine::device::DeviceInfo]| -> Vec<Value> {
        devices
            .iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "channels": d.channels,
                    "default_sample_rate": d.default_sample_rate,
                    "default": d.is_default,
                })
            })
            .collect()
    };
    json!({
        "hosts": hosts,
        "inputs": describe(&inputs),
        "outputs": describe(&outputs),
    })
}

/// Ctrl+C, SIGTERM or a console close stop the server cleanly: the take
/// being recorded is closed and the session saved.
fn install_stop_handler() {
    #[cfg(unix)]
    unsafe {
        extern "C" fn on_signal(_: libc::c_int) {
            STOP.store(true, Ordering::Relaxed);
        }
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    #[cfg(windows)]
    unsafe {
        unsafe extern "system" fn on_ctrl(_: u32) -> windows_sys::core::BOOL {
            STOP.store(true, Ordering::Relaxed);
            // Handled: give the main loop time to finish the take.
            std::thread::sleep(Duration::from_secs(3));
            1
        }
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use livestage_engine::{History, InsertSettings, Scene};
    use std::cell::Cell;
    use std::sync::mpsc::Receiver;

    /// The engine as the server sees it, without a device.
    #[derive(Default)]
    struct FakeMixer {
        session: Session,
        revision: u64,
        history: History,
        differs: Option<bool>,
        /// How often `scene_differs` was asked.
        asked: Cell<usize>,
        applied: Vec<Command>,
        note: Option<String>,
    }

    impl Mixer for FakeMixer {
        fn session(&self) -> &Session {
            &self.session
        }
        fn revision(&self) -> u64 {
            self.revision
        }
        fn history(&self) -> History {
            self.history.clone()
        }
        fn scene_differs(&self, _id: Id) -> Option<bool> {
            self.asked.set(self.asked.get() + 1);
            self.differs
        }
        fn apply_with_note(&mut self, command: Command) -> Result<Option<String>, String> {
            if let Command::SceneStore { name, .. } = &command {
                let id = self.session.allocate_id();
                self.session
                    .scenes
                    .push(scene(id, name.as_deref().unwrap_or("Scene")));
                self.session.current_scene = Some(id);
            }
            self.applied.push(command);
            self.revision += 1;
            Ok(self.note.clone())
        }
        fn save(&mut self, path: &Path, _timeout: Duration) -> Result<(), String> {
            self.session.save(path)
        }
    }

    fn scene(id: Id, name: &str) -> Scene {
        serde_json::from_value(json!({
            "id": id,
            "name": name,
            "note": "quiet intro",
            "mix": {"channels": [{"id": 1, "name": "Kick", "fader_db": -3.0}]},
        }))
        .unwrap()
    }

    fn options(session: Option<PathBuf>) -> Options {
        Options {
            session,
            host: None,
            input: None,
            output: None,
            rate: None,
            buffer: None,
            record_dir: None,
            http: None,
            list_devices: false,
            daemon: false,
            autosave: true,
            autosave_seconds: DEFAULT_AUTOSAVE_SECONDS,
            library: None,
            users: None,
            remote: None,
        }
    }

    /// A server with one web client (id 1), whose messages come out of the
    /// receiver.
    fn server(session: Option<PathBuf>) -> (Server, Receiver<String>) {
        let (outbox, inbox) = std::sync::mpsc::sync_channel(web::OUTBOX);
        let options = options(session);
        let saves = Saves::new(autosave_period(&options), Instant::now(), None);
        let server = Server {
            options,
            clients: vec![(1, outbox)],
            catalog: Vec::new(),
            sent_revision: None,
            sent_history: None,
            scene_light: SceneLight::default(),
            watches: HashMap::new(),
            watched: HashSet::new(),
            storage: storage::Storage::start(Box::new(|_| {})),
            library: library::Library::open(None).0,
            saves,
            users: users::Users::in_memory(16),
            remote: remote::Remote::open(None, [127, 0, 0, 1].into(), None).0,
        };
        (server, inbox)
    }

    fn received(inbox: &Receiver<String>) -> Vec<Value> {
        inbox
            .try_iter()
            .map(|text| serde_json::from_str(&text).unwrap())
            .collect()
    }

    fn of_type<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
        messages.iter().filter(|m| m["type"] == kind).collect()
    }

    fn keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        keys
    }

    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("livestage-server-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_session_message_leaves_scene_mixes_on_the_server() {
        let mut session = Session::default();
        session.scenes = vec![scene(7, "Verse"), scene(8, "Chorus")];
        session.current_scene = Some(8);
        let message = session_message(&session);
        assert_eq!(message["type"], "session");
        let sent = &message["session"];

        let scenes = sent["scenes"].as_array().unwrap();
        assert_eq!(scenes.len(), 2);
        for scene in scenes {
            assert_eq!(keys(scene), ["id", "name", "note", "scope"]);
        }
        assert_eq!(scenes[0]["id"], 7);
        assert_eq!(scenes[0]["name"], "Verse");
        assert_eq!(scenes[0]["note"], "quiet intro");
        assert_eq!(
            scenes[0]["scope"],
            serde_json::to_value(RecallScope::default()).unwrap()
        );

        // Everything else exactly as the session serializes it.
        let mut whole = serde_json::to_value(&session).unwrap();
        let mut view = sent.clone();
        whole.as_object_mut().unwrap().remove("scenes");
        view.as_object_mut().unwrap().remove("scenes");
        assert_eq!(view, whole);
        assert_eq!(sent["current_scene"], 8);
    }

    /// Bus roles and widths, send pans, matrices, the monitor source,
    /// talkback, the oscillator and layers reach the web clients; a scene
    /// holding matrices still goes as its summary only.
    #[test]
    fn the_session_message_carries_buses_matrices_and_monitor_setup() {
        use livestage_engine::{
            BusRole, BusStrip, InjectDest, Layer, MatrixFeed, MatrixSource, MatrixStrip,
            MonitorSource,
        };
        let mut session = Session::default();
        session.buses.push(BusStrip {
            id: 2,
            name: "Mon 1".to_string(),
            role: BusRole::Aux,
            stereo: false,
            ..BusStrip::default()
        });
        session.matrices.push(MatrixStrip {
            id: 3,
            name: "Fill".to_string(),
            sources: vec![MatrixSource {
                source: MatrixFeed::Bus(2),
                level_db: -6.0,
                pan: 0.25,
            }],
            ..MatrixStrip::default()
        });
        session.monitor.source = MonitorSource::Matrix(3);
        session.talkback.to = vec![InjectDest::Bus(2), InjectDest::Monitor];
        session.oscillator.to = vec![InjectDest::Matrix(3)];
        session.layers.push(Layer {
            name: "Outs".to_string(),
            strips: vec![StripRef::Matrix(3), StripRef::Bus(2)],
        });
        let mut stored = scene(7, "Verse");
        stored.mix = session.mix_state();
        session.scenes = vec![stored];
        let message = session_message(&session);
        let sent = &message["session"];
        assert_eq!(sent["buses"][0]["role"], "aux");
        assert_eq!(sent["buses"][0]["stereo"], false);
        assert_eq!(
            sent["matrices"][0]["sources"][0]["source"],
            json!({"kind": "bus", "id": 2})
        );
        assert_eq!(
            sent["monitor"]["source"],
            json!({"kind": "matrix", "id": 3})
        );
        assert_eq!(sent["talkback"]["to"][1], json!({"kind": "monitor"}));
        assert_eq!(sent["talkback"]["hpf"], true);
        assert_eq!(sent["oscillator"]["kind"], "sine");
        assert_eq!(
            sent["layers"][0]["strips"][0],
            json!({"kind": "matrix", "id": 3})
        );
        assert_eq!(keys(&sent["scenes"][0]), ["id", "name", "note", "scope"]);
    }

    #[test]
    fn the_session_is_pushed_when_the_revision_moves() {
        let (mut server, inbox) = server(None);
        let mut mixer = FakeMixer::default();
        server.push_session_if_changed(&mixer);
        assert_eq!(of_type(&received(&inbox), "session").len(), 1);
        server.push_session_if_changed(&mixer);
        assert!(of_type(&received(&inbox), "session").is_empty());

        // Not compared: only the revision says the session changed.
        mixer.session.name = "Changed".into();
        server.push_session_if_changed(&mixer);
        assert!(of_type(&received(&inbox), "session").is_empty());

        mixer.revision += 1;
        server.push_session_if_changed(&mixer);
        let messages = received(&inbox);
        let sessions = of_type(&messages, "session");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session"]["name"], "Changed");
    }

    #[test]
    fn the_history_is_pushed_when_it_changes() {
        let (mut server, inbox) = server(None);
        let mut mixer = FakeMixer::default();
        server.push_history_if_changed(&mixer);
        let messages = received(&inbox);
        let history = of_type(&messages, "history");
        assert_eq!(history.len(), 1);
        assert_eq!(
            *history[0],
            json!({"type": "history", "undo": null, "redo": null, "undo_depth": 0, "redo_depth": 0})
        );
        server.push_history_if_changed(&mixer);
        assert!(received(&inbox).is_empty());

        mixer.history = History {
            undo: Some("Fader Kick".into()),
            redo: None,
            undo_depth: 1,
            redo_depth: 0,
        };
        server.push_history_if_changed(&mixer);
        let messages = received(&inbox);
        assert_eq!(of_type(&messages, "history")[0]["undo"], "Fader Kick");
        assert_eq!(of_type(&messages, "history")[0]["undo_depth"], 1);
    }

    #[test]
    fn the_scene_light_is_sent_on_change_and_worked_out_only_when_needed() {
        let (mut server, inbox) = server(None);
        let mut mixer = FakeMixer::default();
        // No current scene: not modified, nothing to ask.
        server.push_scene_state_if_changed(&mixer);
        let messages = received(&inbox);
        assert_eq!(
            *of_type(&messages, "scene_state")[0],
            json!({"type": "scene_state", "current": null, "modified": false})
        );
        assert_eq!(mixer.asked.get(), 0);

        mixer.session.scenes.push(scene(4, "Intro"));
        mixer.session.current_scene = Some(4);
        mixer.differs = Some(false);
        mixer.revision += 1;
        server.push_scene_state_if_changed(&mixer);
        let messages = received(&inbox);
        assert_eq!(of_type(&messages, "scene_state")[0]["current"], 4);
        assert_eq!(of_type(&messages, "scene_state")[0]["modified"], false);
        assert_eq!(mixer.asked.get(), 1);

        // Nothing moved: not asked again, nothing sent.
        for _ in 0..5 {
            server.push_scene_state_if_changed(&mixer);
        }
        assert_eq!(mixer.asked.get(), 1);
        assert!(received(&inbox).is_empty());

        // A fader moved: asked again; the light comes on once.
        mixer.differs = Some(true);
        mixer.revision += 1;
        server.push_scene_state_if_changed(&mixer);
        mixer.revision += 1;
        server.push_scene_state_if_changed(&mixer);
        let messages = received(&inbox);
        let states = of_type(&messages, "scene_state");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0]["modified"], true);
        assert_eq!(mixer.asked.get(), 3);

        // The scene deleted under it: no light.
        mixer.differs = None;
        mixer.session.current_scene = None;
        server.push_scene_state_if_changed(&mixer);
        let messages = received(&inbox);
        assert_eq!(of_type(&messages, "scene_state")[0]["modified"], false);
    }

    #[test]
    fn applying_a_library_item_pastes_it_and_saving_one_tells_everyone() {
        let (mut server, inbox) = server(None);
        let mut mixer = FakeMixer::default();
        let reply = library_command(
            &mut mixer,
            &mut server,
            "library_apply",
            json!({"cmd": "library_apply", "item": "factory-vocal",
                   "targets": [{"kind": "channel", "id": 2}, {"kind": "master"}]}),
            Source::Web(1),
        );
        assert_eq!(reply, json!({"ok": true}));
        let factory = server.library.get("factory-vocal").unwrap().clone();
        assert_eq!(
            mixer.applied,
            vec![Command::PasteStrip {
                targets: vec![StripRef::Channel(2), StripRef::Master],
                settings: factory.settings,
                sections: factory.sections,
            }]
        );
        assert!(
            of_type(&received(&inbox), "library").is_empty(),
            "applying changes no item"
        );

        let reply = library_command(
            &mut mixer,
            &mut server,
            "library_save",
            json!({"cmd": "library_save", "name": "Lead vox", "category": "Vocal",
                   "settings": {"fader_db": -4.0, "processing": {"hpf": {"on": true, "hz": 120}}},
                   "sections": ["hpf", "fader_pan"]}),
            Source::Web(1),
        );
        assert_eq!(reply["ok"], true, "{reply}");
        let id = reply["item"].as_str().unwrap().to_string();
        let messages = received(&inbox);
        let library = of_type(&messages, "library");
        assert_eq!(library.len(), 1);
        assert_eq!(library[0]["items"].as_array().unwrap().len(), 7);

        let reply = library_command(
            &mut mixer,
            &mut server,
            "library_rename",
            json!({"item": id, "name": "Lead vocal", "category": "Vox"}),
            Source::Web(1),
        );
        assert_eq!(reply, json!({"ok": true}));
        assert_eq!(server.library.get(&id).unwrap().name, "Lead vocal");
        let reply = library_command(
            &mut mixer,
            &mut server,
            "library_delete",
            json!({"item": "factory-kick"}),
            Source::Web(1),
        );
        assert_eq!(reply["ok"], false);
        assert!(reply["error"].as_str().unwrap().contains("read-only"));
        let reply = library_command(
            &mut mixer,
            &mut server,
            "library_delete",
            json!({"item": id}),
            Source::Web(1),
        );
        assert_eq!(reply, json!({"ok": true}));
        // Rename and delete were pushed; the refusal was not.
        assert_eq!(of_type(&received(&inbox), "library").len(), 2);

        let reply = library_command(
            &mut mixer,
            &mut server,
            "library",
            json!({"cmd": "library"}),
            Source::Stdin,
        );
        assert_eq!(reply["items"].as_array().unwrap().len(), 6);
        let reply = library_command(
            &mut mixer,
            &mut server,
            "library_apply",
            json!({"item": "factory-vocal"}),
            Source::Stdin,
        );
        assert_eq!(reply["ok"], false, "targets are required");
    }

    #[test]
    fn a_web_client_keeps_plugin_state_only_when_copied_from_this_session() {
        let (mut server, _inbox) = server(None);
        let mut mixer = FakeMixer::default();
        let plugin = |state: Option<(&str, &str)>| InsertPlugin::External {
            format: "VST3".into(),
            path: "C:/Plugins/Comp.vst3".into(),
            class_id: "ABCD".into(),
            name: "Comp".into(),
            state: state.map(|(a, b)| (a.to_string(), b.to_string())),
        };
        let mut channel = livestage_engine::ChannelStrip::new(1, "Vox".into(), Default::default());
        channel.core.inserts.push(livestage_engine::InsertSlot {
            id: 2,
            bypass: false,
            plugin: plugin(Some(("c2Vzc2lvbg==", ""))),
        });
        mixer.session.channels.push(channel);
        #[cfg(feature = "external-plugins")]
        {
            server.catalog = vec![json!({"path": "C:/Plugins/Comp.vst3", "class_id": "ABCD"})];
        }

        let mut settings = StripSettings {
            inserts: Some(vec![
                InsertSettings {
                    bypass: false,
                    plugin: plugin(Some(("c2Vzc2lvbg==", ""))),
                },
                InsertSettings {
                    bypass: true,
                    plugin: plugin(Some(("bWFkZSB1cA==", ""))),
                },
            ]),
            ..StripSettings::default()
        };
        vet_settings(&mixer, &mut server, &mut settings, Source::Web(1)).unwrap();
        let states: Vec<bool> = settings
            .inserts
            .as_ref()
            .unwrap()
            .iter()
            .map(|i| matches!(&i.plugin, InsertPlugin::External { state: Some(_), .. }))
            .collect();
        assert_eq!(
            states,
            [true, false],
            "the copied state stays, the made-up one goes"
        );

        #[cfg(feature = "external-plugins")]
        {
            let mut elsewhere = StripSettings {
                inserts: Some(vec![InsertSettings {
                    bypass: false,
                    plugin: InsertPlugin::External {
                        format: "VST3".into(),
                        path: "C:/Elsewhere/Evil.vst3".into(),
                        class_id: "EEEE".into(),
                        name: "Evil".into(),
                        state: None,
                    },
                }]),
                ..StripSettings::default()
            };
            assert!(vet_settings(&mixer, &mut server, &mut elsewhere, Source::Web(1)).is_err());
        }

        // stdin is trusted as it is.
        let mut trusted = StripSettings {
            inserts: Some(vec![InsertSettings {
                bypass: false,
                plugin: plugin(Some(("bWFkZSB1cA==", ""))),
            }]),
            ..StripSettings::default()
        };
        let before = trusted.clone();
        vet_settings(&mixer, &mut server, &mut trusted, Source::Stdin).unwrap();
        assert_eq!(trusted, before);
    }

    #[test]
    fn a_reply_carries_the_engines_note() {
        let (mut server, _inbox) = server(None);
        let mut mixer = FakeMixer {
            note: Some("2 inserts differ from the scene and were left as they are".into()),
            ..FakeMixer::default()
        };
        let reply = run(&mut mixer, &mut server, Command::SceneRecall { id: 3 });
        assert_eq!(
            reply,
            json!({"ok": true, "note": "2 inserts differ from the scene and were left as they are"})
        );
        mixer.note = None;
        assert_eq!(
            run(&mut mixer, &mut server, Command::Undo),
            json!({"ok": true})
        );
    }

    #[test]
    fn storing_a_scene_saves_the_session_at_once() {
        let dir = folder("scene-store");
        let path = dir.join("show.json");
        let (mut server, inbox) = server(Some(path.clone()));
        let mut mixer = FakeMixer::default();
        mixer.revision = 5;
        let reply = run(
            &mut mixer,
            &mut server,
            Command::SceneStore {
                id: None,
                name: Some("Verse".into()),
            },
        );
        assert_eq!(reply, json!({"ok": true}));
        let saved = Session::load(&path).unwrap();
        assert_eq!(saved.scenes.len(), 1);
        assert_eq!(saved.scenes[0].name, "Verse");
        let messages = received(&inbox);
        let saves = of_type(&messages, "saved");
        assert_eq!(saves.len(), 1);
        assert_eq!(keys(saves[0]), ["at", "auto", "path", "type"]);
        assert_eq!(saves[0]["auto"], true);
        assert_eq!(saves[0]["path"], json!(path));
        assert!(!server.saves.unsaved(mixer.revision));
        assert_eq!(server.saves.last.as_ref().unwrap()["auto"], true);

        // Other edits are not saved at once.
        run(
            &mut mixer,
            &mut server,
            Command::SetFader {
                strip: StripRef::Master,
                db: -3.0,
            },
        );
        assert!(of_type(&received(&inbox), "saved").is_empty());
        assert!(server.saves.unsaved(mixer.revision));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_autosave_means_no_save_after_a_scene_store() {
        let dir = folder("no-autosave");
        let path = dir.join("show.json");
        let (mut server, inbox) = server(Some(path.clone()));
        server.options.autosave = false;
        let mut mixer = FakeMixer::default();
        run(
            &mut mixer,
            &mut server,
            Command::SceneStore {
                id: None,
                name: None,
            },
        );
        assert!(!path.exists());
        assert!(of_type(&received(&inbox), "saved").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autosave_runs_once_a_period_and_only_when_something_changed() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut saves = Saves::new(Some(s(30)), t0, Some(1));
        // Not yet due, then due but nothing changed.
        assert!(!saves.due(t0 + s(29), 2));
        assert!(!saves.due(t0 + s(30), 1));
        // Changed, but the next period has only just started.
        assert!(!saves.due(t0 + s(45), 2));
        assert!(saves.due(t0 + s(60), 2));
        saves.saved(2, json!({}));
        assert!(!saves.unsaved(2));
        // Saved: the next look finds nothing to do.
        assert!(!saves.due(t0 + s(90), 2));
        // A failed save is tried again a period later, not on every loop.
        assert!(saves.due(t0 + s(120), 3));
        assert!(!saves.due(t0 + s(121), 3));
        assert!(!saves.due(t0 + s(149), 3));
        assert!(saves.due(t0 + s(150), 3));

        // Off: never due.
        let mut off = Saves::new(None, t0, None);
        assert!(!off.due(t0 + s(3600), 9));
        assert!(off.unsaved(9), "a session never saved is unsaved");
    }

    #[test]
    fn a_due_autosave_writes_the_file_and_tells_the_clients() {
        let dir = folder("autosave");
        let path = dir.join("show.json");
        let (mut server, inbox) = server(Some(path.clone()));
        let t0 = Instant::now();
        server.saves = Saves::new(Some(Duration::from_secs(30)), t0, Some(0));
        let mut mixer = FakeMixer::default();
        mixer.session.name = "Friday".into();
        mixer.revision = 1;
        assert!(
            !server
                .saves
                .due(t0 + Duration::from_secs(10), mixer.revision)
        );
        assert!(
            server
                .saves
                .due(t0 + Duration::from_secs(30), mixer.revision)
        );
        server.autosave(&mut mixer);
        assert_eq!(Session::load(&path).unwrap().name, "Friday");
        let messages = received(&inbox);
        assert_eq!(of_type(&messages, "saved")[0]["auto"], true);
        assert!(
            !server
                .saves
                .due(t0 + Duration::from_secs(60), mixer.revision)
        );

        // A save asked for is not an autosave.
        mixer.revision = 2;
        server.save_session(&mut mixer, &path, false).unwrap();
        assert_eq!(of_type(&received(&inbox), "saved")[0]["auto"], false);
        assert!(!server.saves.unsaved(2));
        // Saving elsewhere leaves the session file's state alone.
        mixer.revision = 3;
        server
            .save_session(&mut mixer, &dir.join("copy.json"), false)
            .unwrap();
        assert!(server.saves.unsaved(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_library_sits_next_to_the_session_unless_told() {
        let mut options = options(Some(PathBuf::from("/data/livestage/show.json")));
        assert_eq!(
            library_path(&options),
            Some(PathBuf::from("/data/livestage/library.json"))
        );
        options.session = Some(PathBuf::from("show.json"));
        assert_eq!(library_path(&options), Some(PathBuf::from("library.json")));
        options.library = Some(PathBuf::from("/srv/strips.json"));
        assert_eq!(
            library_path(&options),
            Some(PathBuf::from("/srv/strips.json"))
        );
        options.library = None;
        options.session = None;
        assert_eq!(library_path(&options), None);
    }

    #[test]
    fn the_autosave_period_follows_the_flags() {
        let mut options = options(Some(PathBuf::from("show.json")));
        assert_eq!(autosave_period(&options), Some(Duration::from_secs(30)));
        options.autosave_seconds = 5;
        assert_eq!(autosave_period(&options), Some(Duration::from_secs(5)));
        options.autosave_seconds = 0;
        assert_eq!(autosave_period(&options), None);
        options.autosave_seconds = 30;
        options.autosave = false;
        assert_eq!(autosave_period(&options), None);
        options.autosave = true;
        options.session = None;
        assert_eq!(autosave_period(&options), None);
    }

    #[test]
    fn a_request_tag_travels_as_req_when_the_command_has_its_own_id() {
        let recall = json!({"cmd": "scene_recall", "id": 3, "req": 1_000_007});
        assert_eq!(request_tag(&recall), Some(json!(1_000_007)));
        let fader = json!({"cmd": "set_fader", "id": 1_000_008});
        assert_eq!(request_tag(&fader), Some(json!(1_000_008)));
        assert_eq!(request_tag(&json!({"cmd": "status"})), None);
        let reply = web_reply(json!({"ok": true}), request_tag(&recall));
        assert_eq!(
            reply,
            json!({"type": "reply", "ok": true, "id": 1_000_007, "req": 1_000_007})
        );
        // The scene's id is the command's, not the request's.
        let mut mixer = FakeMixer::default();
        let (mut server, _inbox) = server(None);
        let reply = engine_command(&mut mixer, &mut server, recall, Source::Web(1));
        assert_eq!(reply, json!({"ok": true}));
        assert_eq!(mixer.applied, vec![Command::SceneRecall { id: 3 }]);
    }

    #[test]
    fn save_times_are_rfc3339_utc() {
        let at = |secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(rfc3339(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(at(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(at(1_000_000_000)), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339(at(1_700_000_000)), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(at(4_102_444_799)), "2099-12-31T23:59:59Z");
        let message = saved_message(Path::new("show.json"), at(0), true);
        assert_eq!(
            message,
            json!({"type": "saved", "path": "show.json", "at": "1970-01-01T00:00:00Z", "auto": true})
        );
    }

    /// A 16-bit mono WAV of `frames` frames of silence at 48 kHz.
    fn write_wav(path: &Path, frames: u32) {
        let data = frames * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&96_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data.to_le_bytes());
        bytes.resize(bytes.len() + data as usize, 0);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn takes_lists_the_recordings_folder_and_a_web_load_must_name_one() {
        let root = folder("takes");
        let take = root.join("Take 2026-10-10 20-00-00 UTC");
        std::fs::create_dir_all(&take).unwrap();
        write_wav(&take.join("Kick.wav"), 4800);
        // Not a take: no audio in it.
        std::fs::create_dir_all(root.join("notes")).unwrap();
        let elsewhere = folder("takes-elsewhere");
        write_wav(&elsewhere.join("Kick.wav"), 4800);

        let mut mixer = FakeMixer::default();
        mixer.session.recording.folder = Some(root.clone());
        let reply = takes_reply(&mixer);
        assert_eq!(reply["ok"], true);
        let takes = reply["takes"].as_array().unwrap();
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0]["path"], json!(take));
        assert_eq!(
            takes[0]["files"],
            json!([{"name": "Kick.wav", "channels": 1, "rate": 48000, "seconds": 0.1}])
        );

        let (mut server, _inbox) = server(None);
        let load = |folder: &Path| Command::PlaybackLoad {
            folder: folder.to_path_buf(),
        };
        for refused in [
            elsewhere.clone(),
            root.join("notes"),
            take.join(".."),
            root.clone(),
        ] {
            let error = admit(&mixer, &mut server, load(&refused), Source::Web(1)).unwrap_err();
            assert!(error.contains("is not a take"), "{error}");
        }
        assert_eq!(
            admit(&mixer, &mut server, load(&take), Source::Web(1)).unwrap(),
            load(&take)
        );
        // The console may load any folder.
        assert_eq!(
            admit(&mixer, &mut server, load(&elsewhere), Source::Stdin).unwrap(),
            load(&elsewhere)
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }
}
