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
//! ```
//!
//! A web client sends the same commands, with an optional `"id"` echoed in
//! its reply, and is pushed the session, the status and the meters as they
//! change. It cannot quit the server, save anywhere but the session file,
//! move the recording folder, or load a plug-in the catalog does not list.

mod web;

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, SyncSender};
use std::time::{Duration, Instant};

use livestage_engine::{Command, Id, InsertPlugin, InsertState, LiveEngine, Session};
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
    autosave: bool,
}

const USAGE: &str = "usage: livestage-server [--session FILE] [--host NAME] [--input DEVICE] \
[--output DEVICE] [--rate HZ] [--buffer FRAMES] [--record-dir DIR] [--http ADDR:PORT | --no-http] \
[--daemon] [--no-autosave] [--list-devices]";

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

    let mut server = Server {
        options,
        clients: Vec::new(),
        catalog: Vec::new(),
        sent_session: None,
        watches: HashMap::new(),
        watched: HashSet::new(),
    };
    let stdout = std::io::stdout();
    let start = Instant::now();
    let (mut next_meters, mut next_session, mut next_status) = (start, start, start);
    loop {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        engine.poll();

        let now = Instant::now();
        if !server.clients.is_empty() {
            if now >= next_meters {
                next_meters = now + METER_PERIOD;
                server.broadcast(&meters_message(&engine));
                server.push_telemetry(&engine);
            }
            if now >= next_session {
                next_session = now + SESSION_PERIOD;
                server.push_session_if_changed(&engine);
            }
            if now >= next_status {
                next_status = now + STATUS_PERIOD;
                server.broadcast(&status_message(&engine));
            }
        }
        let wake = if server.clients.is_empty() {
            now + Duration::from_millis(50)
        } else {
            next_meters.min(next_session).min(next_status)
        };

        match inbound.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            Ok(Inbound::Stdin(line)) => {
                let (reply, quit) = handle_line(&mut engine, &mut server, &line, Source::Stdin);
                let mut out = stdout.lock();
                let _ = writeln!(out, "{reply}");
                let _ = out.flush();
                if quit {
                    break;
                }
            }
            Ok(Inbound::StdinClosed) => break,
            Ok(Inbound::Web(id, text)) => {
                let request_id = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("id").cloned());
                let (mut reply, _) = handle_line(&mut engine, &mut server, &text, Source::Web(id));
                if let Value::Object(fields) = &mut reply {
                    fields.insert("type".to_string(), json!("reply"));
                    if let Some(request_id) = request_id {
                        fields.insert("id".to_string(), request_id);
                    }
                }
                server.send_to(id, &reply);
            }
            Ok(Inbound::Joined(id, outbox)) => {
                // Bring the others up to now first, so that what the new
                // client is sent is what everyone has.
                server.push_session_if_changed(&engine);
                server.welcome(&engine, &outbox);
                server.clients.push((id, outbox));
            }
            Ok(Inbound::Left(id)) => {
                server.clients.retain(|(client, _)| *client != id);
                server.watches.remove(&id);
            }
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
        if let Some(path) = &server.options.session {
            match engine.save(path, Duration::from_secs(2)) {
                Ok(()) => eprintln!("[livestage] session saved to {}", path.display()),
                Err(error) => eprintln!("[livestage] session not saved: {error}"),
            }
        }
    }
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
/// anyone who can reach the port.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Stdin,
    Web(ClientId),
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
    /// The session as the web clients last saw it.
    sent_session: Option<Session>,
}

impl Server {
    /// Queue `message` for every client. One whose outbox is full has
    /// stopped reading: it is dropped, and reconnects to a fresh snapshot.
    fn broadcast(&mut self, message: &Value) {
        let text = message.to_string();
        self.clients
            .retain(|(_, outbox)| outbox.try_send(text.clone()).is_ok());
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
                    .is_some_and(|inserts| inserts.contains(insert));
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

    fn push_session_if_changed(&mut self, engine: &LiveEngine) {
        if self.sent_session.as_ref() == Some(engine.session()) {
            return;
        }
        self.sent_session = Some(engine.session().clone());
        self.broadcast(&session_message(engine));
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
            "effects": effects,
        });
        for message in [hello, session_message(engine), status_message(engine)] {
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

fn session_message(engine: &LiveEngine) -> Value {
    json!({"type": "session", "session": engine.session()})
}

fn status_message(engine: &LiveEngine) -> Value {
    // Only inserts that are not simply running: a missing id is ready.
    let mut inserts = serde_json::Map::new();
    let session = engine.session();
    let cores = session
        .channels
        .iter()
        .map(|c| &c.core)
        .chain(session.buses.iter().map(|b| &b.core))
        .chain(std::iter::once(&session.master.core));
    for core in cores {
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
    })
}

fn meters_message(engine: &LiveEngine) -> Value {
    let meters: Vec<Value> = engine
        .meters()
        .into_iter()
        .map(|(strip, levels)| json!({"strip": strip, "levels": levels}))
        .collect();
    json!({"type": "meters", "meters": meters})
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
    mut value: Value,
    source: Source,
) -> (Value, bool) {
    let web = matches!(source, Source::Web(_));
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
            Source::Stdin => json!({"ok": false, "error": "watch_insert is for web clients"}),
        },
        Some("quit") if web => {
            json!({"ok": false, "error": "quit is only taken on the server's console"})
        }
        Some("quit") => return (json!({"ok": true}), true),
        Some("status") => json!({"ok": true, "status": engine.status()}),
        Some("session") => json!({"ok": true, "session": engine.session()}),
        Some("meters") => {
            let meters = meters_message(engine)["meters"].take();
            json!({"ok": true, "meters": meters})
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
            let session = engine.session();
            let cores = session
                .channels
                .iter()
                .map(|c| (json!({"kind": "channel", "id": c.id}), &c.core))
                .chain(
                    session
                        .buses
                        .iter()
                        .map(|b| (json!({"kind": "bus", "id": b.id}), &b.core)),
                )
                .chain(std::iter::once((
                    json!({"kind": "master"}),
                    &session.master.core,
                )));
            let inserts: Vec<Value> = cores
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
                    Some(path) => match engine.save(&path, Duration::from_secs(2)) {
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
        Some("stop_recording") => match engine.stop_recording() {
            Some(summary) => {
                let mut reply = summary_json(&summary);
                reply["ok"] = json!(true);
                reply
            }
            None => json!({"ok": false, "error": "not recording"}),
        },
        _ => {
            if let Some(id) = value.as_object_mut() {
                id.remove("id");
            }
            match serde_json::from_value::<Command>(value) {
                Ok(command) => match admit(engine, server, command, source) {
                    Ok(command) => match engine.apply(command) {
                        Ok(()) => json!({"ok": true}),
                        Err(error) => json!({"ok": false, "error": error}),
                    },
                    Err(error) => json!({"ok": false, "error": error}),
                },
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        }
    };
    (reply, false)
}

/// What a web client may not do with an engine command: load a plug-in
/// binary the catalog does not list (or hand it state of its own), or point
/// the recorder at another folder. stdin is trusted as it is.
fn admit(
    engine: &LiveEngine,
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
        _ => {}
    }
    Ok(command)
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
