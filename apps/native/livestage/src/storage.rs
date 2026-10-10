//! The appliance's disks, as its storage service reports them.
//!
//! On the LiveStage appliance a root service (`livestage-setup
//! --storage-service`) owns the disks: it mounts them, ejects and formats
//! them, and says where recordings go. This server asks it over a Unix
//! socket ([`storage_api`]) from a worker thread of its own — every request
//! can take time (a format takes minutes), and neither the audio nor the
//! command loop may wait on it. The worker lists the disks every few
//! seconds and hands each answer back to the command loop as a [`Done`].
//!
//! What needs the mixer's state is decided here, in the command loop: no
//! changing the recording target, ejecting it or formatting its disk while
//! a take is being written, and no take started while such a change is
//! under way. The loop also keeps the recorder's folder on the target's
//! `recordings_dir` ([`Storage::folder_to_apply`]), never in the middle of
//! a take.
//!
//! Anywhere else (a desktop, a dev machine) there is no service: the state
//! says storage is not available here and the recorder keeps its folder.

// The worker and its transport only run where the service can exist (and
// in the tests).
#![cfg_attr(not(unix), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::storage_api::{self as api, Request, State};
use crate::web::ClientId;

/// How often the worker lists the disks.
pub const POLL: Duration = Duration::from_secs(3);

/// Who asked: a web client (`Some`) or stdin (`None`), and the request id
/// to echo.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    pub client: Option<ClientId>,
    pub request: Option<Value>,
}

/// Why a call got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// No service here (no socket, not allowed to use it, not this OS).
    Unavailable(String),
    /// The service is there but this call failed (timeout, bad reply).
    Failed(String),
}

impl CallError {
    fn text(&self) -> &str {
        match self {
            CallError::Unavailable(text) | CallError::Failed(text) => text,
        }
    }
}

/// One request, one reply.
pub trait Transport: Send + 'static {
    fn call(&mut self, request: &Request) -> Result<api::Reply, CallError>;
}

/// What the worker hands back to the command loop.
#[derive(Debug)]
pub enum Done {
    /// The periodic list.
    Listed(Result<api::Reply, CallError>),
    /// The answer to a request someone made.
    Answered {
        request: Request,
        origin: Origin,
        result: Result<api::Reply, CallError>,
    },
}

struct Job {
    request: Request,
    origin: Origin,
}

pub type Notify = Box<dyn Fn(Done) + Send + 'static>;

/// The command loop's side: the last state, and the request under way.
pub struct Storage {
    jobs: Option<Sender<Job>>,
    state: Option<State>,
    /// Why there is no state, when there is none to show.
    unavailable: Option<String>,
    /// The change under way (use, eject or format).
    busy: Option<Request>,
    /// The `storage` message the clients last got.
    sent: Option<Value>,
}

impl Storage {
    /// Ask this machine's storage service, if it has one.
    pub fn start(notify: Notify) -> Self {
        #[cfg(unix)]
        {
            Self::with_transport(unix::Socket::new(api::SOCKET), POLL, notify)
        }
        #[cfg(not(unix))]
        {
            drop(notify);
            Self {
                jobs: None,
                state: None,
                unavailable: Some("Storage is not available on this machine.".to_string()),
                busy: None,
                sent: None,
            }
        }
    }

    /// Run a worker on `transport`, listing every `poll`.
    pub fn with_transport(transport: impl Transport, poll: Duration, notify: Notify) -> Self {
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("livestage-storage".to_string())
            .spawn(move || worker(transport, queue, poll, notify));
        let (jobs, unavailable) = match spawned {
            Ok(_) => (Some(jobs), Some("Asking the storage service…".to_string())),
            Err(error) => (
                None,
                Some(format!("The storage worker did not start: {error}")),
            ),
        };
        Self {
            jobs,
            state: None,
            unavailable,
            busy: None,
            sent: None,
        }
    }

    /// Why a take must not start now, if it must not.
    pub fn blocks_recording(&self) -> Option<String> {
        self.busy.as_ref().map(|request| match request {
            Request::Format { .. } => "Wait for the format to finish before recording.".to_string(),
            Request::Eject { .. } => "Wait for the eject to finish before recording.".to_string(),
            _ => "Wait for the recording disk to change before recording.".to_string(),
        })
    }

    /// A web client's or stdin's `{"cmd":"storage","op":…}`, checked
    /// against the mixer's state and queued. `Err` is the refusal to send
    /// back now; `Ok` means the answer comes later as a [`Done`].
    pub fn ask(&mut self, command: &Value, recording: bool, origin: Origin) -> Result<(), String> {
        let request = parse_command(command)?;
        let Some(jobs) = &self.jobs else {
            return Err(self.unavailable_text());
        };
        if self.state.is_none() {
            return Err(self.unavailable_text());
        }
        if let Some(busy) = &self.busy {
            return Err(format!(
                "Busy: {}. Try again when it is done.",
                describe(busy)
            ));
        }
        if let Some(refusal) = refusal(&request, self.state.as_ref(), recording) {
            return Err(refusal);
        }
        jobs.send(Job {
            request: request.clone(),
            origin,
        })
        .map_err(|_| "The storage worker stopped.".to_string())?;
        self.busy = Some(request);
        Ok(())
    }

    /// Take the worker's answer. Returns the reply owed to whoever asked.
    pub fn take(&mut self, done: Done) -> Option<(Origin, Value)> {
        match done {
            Done::Listed(result) => {
                self.absorb(&result);
                None
            }
            Done::Answered {
                request,
                origin,
                result,
            } => {
                self.absorb(&result);
                if self.busy.as_ref() == Some(&request) {
                    self.busy = None;
                }
                let reply = match result {
                    Ok(reply) => json!({
                        "ok": reply.ok,
                        "error": reply.error,
                        "storage": reply.storage,
                    }),
                    Err(error) => json!({"ok": false, "error": error.text()}),
                };
                Some((origin, reply))
            }
        }
    }

    fn absorb(&mut self, result: &Result<api::Reply, CallError>) {
        match result {
            Ok(reply) => {
                self.state = Some(reply.storage.clone());
                self.unavailable = None;
            }
            // A request that timed out says nothing about the disks; the
            // next list does.
            Err(CallError::Failed(_)) => {}
            Err(CallError::Unavailable(why)) => {
                self.state = None;
                self.unavailable = Some(why.clone());
            }
        }
    }

    fn unavailable_text(&self) -> String {
        self.unavailable
            .clone()
            .unwrap_or_else(|| "Storage is not available.".to_string())
    }

    /// The folder the recorder should move to now, if it should: the
    /// target's `recordings_dir`, once it differs from `current` and no
    /// take is being written.
    pub fn folder_to_apply(&self, current: Option<&Path>, recording: bool) -> Option<PathBuf> {
        if recording {
            return None;
        }
        let dir = &self.state.as_ref()?.target.recordings_dir;
        if dir.is_empty() || current == Some(Path::new(dir)) {
            return None;
        }
        Some(PathBuf::from(dir))
    }

    /// What the clients are shown: the state (or why there is none), the
    /// change under way, and the folder the recorder writes to.
    pub fn message(&self, folder: &Path) -> Value {
        json!({
            "type": "storage",
            "available": self.state.is_some(),
            "reason": if self.state.is_some() { None } else { Some(self.unavailable_text()) },
            "storage": self.state,
            "busy": self.busy,
            "folder": folder,
        })
    }

    /// [`Self::message`] when it differs from the one last sent.
    pub fn message_if_changed(&mut self, folder: &Path) -> Option<Value> {
        let message = self.message(folder);
        if self.sent.as_ref() == Some(&message) {
            return None;
        }
        self.sent = Some(message.clone());
        Some(message)
    }
}

/// `{"cmd":"storage","op":"use","volume":"…"}`, `…"op":"eject","volume":"…"`,
/// `…"op":"format","disk":"sdb","label":"LIVESTAGE"`. (`id` is the request
/// id on this socket, so the volume goes as `volume`.)
fn parse_command(command: &Value) -> Result<Request, String> {
    let field = |name: &str| {
        command
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let volume = || {
        field("volume")
            .filter(|id| api::valid_id(id))
            .ok_or("storage use and eject need \"volume\": internal or a volume's UUID")
    };
    match command.get("op").and_then(Value::as_str) {
        Some("use") => Ok(Request::Use { id: volume()? }),
        Some("eject") => Ok(Request::Eject { id: volume()? }),
        Some("format") => {
            let disk = field("disk").ok_or("storage format needs \"disk\"")?;
            let label = field("label").unwrap_or_else(|| api::DEFAULT_LABEL.to_string());
            api::valid_label(&label)?;
            Ok(Request::Format { disk, label })
        }
        _ => Err("storage needs \"op\": use, eject or format".to_string()),
    }
}

/// The server's own rules, before the service is asked: nothing that moves
/// or removes the disk a take is being written to.
pub fn refusal(request: &Request, state: Option<&State>, recording: bool) -> Option<String> {
    if let (Request::Format { disk, .. }, Some(state)) = (request, state) {
        if state.disks.iter().any(|d| &d.disk == disk && d.system) {
            return Some("That disk holds the LiveStage system; it cannot be formatted.".into());
        }
    }
    if !recording {
        return None;
    }
    let Some(state) = state else {
        return Some("Stop recording first: the disks are not known yet.".to_string());
    };
    match request {
        Request::List => None,
        Request::Use { .. } => Some("Stop recording before choosing where to record.".to_string()),
        Request::Eject { id } if *id == state.target.id => Some(format!(
            "Stop recording before ejecting {}: the take is being written to it.",
            state.target.label
        )),
        Request::Eject { .. } => None,
        Request::Format { disk, .. } if target_disk(state) == Some(disk.as_str()) => Some(format!(
            "Stop recording before formatting {disk}: the take is being written to it."
        )),
        Request::Format { .. } => None,
    }
}

/// The disk the recording target's volume is on, when it is plugged in.
fn target_disk(state: &State) -> Option<&str> {
    state.volume(&state.target.id).map(|v| v.disk.as_str())
}

fn describe(request: &Request) -> String {
    match request {
        Request::List => "listing the disks".to_string(),
        Request::Use { id } => format!("switching the recording disk to {id}"),
        Request::Eject { id } => format!("ejecting {id}"),
        Request::Format { disk, .. } => format!("formatting {disk}"),
    }
}

fn worker(
    mut transport: impl Transport,
    queue: std::sync::mpsc::Receiver<Job>,
    poll: Duration,
    notify: Notify,
) {
    let mut next_list = Instant::now();
    loop {
        match queue.recv_timeout(next_list.saturating_duration_since(Instant::now())) {
            Ok(Job { request, origin }) => {
                let result = transport.call(&request);
                notify(Done::Answered {
                    request,
                    origin,
                    result,
                });
            }
            Err(RecvTimeoutError::Timeout) => {
                notify(Done::Listed(transport.call(&Request::List)));
                next_list = Instant::now() + poll;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// How long one request may take before the server stops waiting.
pub fn timeout(request: &Request) -> Duration {
    match request {
        Request::List => Duration::from_secs(3),
        Request::Use { .. } => Duration::from_secs(30),
        // Unmounting flushes what is cached for the disk.
        Request::Eject { .. } => Duration::from_secs(90),
        // Partitioning, mkfs and mounting a large, slow stick.
        Request::Format { .. } => Duration::from_secs(600),
    }
}

#[cfg(unix)]
pub mod unix {
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;

    use super::{CallError, Transport, timeout};
    use crate::storage_api::{Reply, Request};

    /// The longest reply line taken.
    const MAX_REPLY: u64 = 4 * 1024 * 1024;

    /// One connection per request, to the service's socket.
    pub struct Socket {
        path: PathBuf,
    }

    impl Socket {
        pub fn new(path: impl Into<PathBuf>) -> Self {
            Self { path: path.into() }
        }
    }

    impl Transport for Socket {
        fn call(&mut self, request: &Request) -> Result<Reply, CallError> {
            let stream = UnixStream::connect(&self.path).map_err(|error| match error.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => CallError::Unavailable(
                    "The storage service is not running on this machine.".to_string(),
                ),
                ErrorKind::PermissionDenied => CallError::Unavailable(
                    "This server may not use the storage service.".to_string(),
                ),
                _ => CallError::Unavailable(format!("The storage service: {error}")),
            })?;
            let wait = timeout(request);
            let failed = |what: &str, error: std::io::Error| {
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                    CallError::Failed("The storage service did not answer in time.".to_string())
                } else {
                    CallError::Failed(format!("The storage service ({what}): {error}"))
                }
            };
            stream
                .set_write_timeout(Some(wait))
                .and_then(|()| stream.set_read_timeout(Some(wait)))
                .map_err(|e| failed("socket", e))?;
            let mut line = serde_json::to_string(request)
                .map_err(|e| CallError::Failed(format!("request not written: {e}")))?;
            line.push('\n');
            (&stream)
                .write_all(line.as_bytes())
                .map_err(|e| failed("send", e))?;
            let mut reply = String::new();
            BufReader::new((&stream).take(MAX_REPLY))
                .read_line(&mut reply)
                .map_err(|e| failed("reply", e))?;
            if reply.trim().is_empty() {
                return Err(CallError::Failed(
                    "The storage service closed without answering.".to_string(),
                ));
            }
            serde_json::from_str(&reply).map_err(|e| {
                CallError::Failed(format!("The storage service's reply did not read: {e}"))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_api::{Disk, Mount, Reply, Target, Volume};
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex};

    fn volume(id: &str, disk: &str, label: &str) -> Volume {
        Volume {
            id: id.into(),
            label: label.into(),
            fs: Some("exfat".into()),
            device: format!("{disk}1"),
            disk: disk.into(),
            model: Some("Stick".into()),
            size_bytes: 64_000_000_000,
            free_bytes: Some(60_000_000_000),
            mounted: Some(Mount::Ro),
            mount_path: Some(format!("/media/{id}")),
            supported: true,
            ejected: false,
        }
    }

    /// Recording to the stick `1A2B-3C4D` on sdb; the system is on sda.
    fn on_stick(available: bool) -> State {
        State {
            target: Target {
                id: "1A2B-3C4D".into(),
                label: "SHOW".into(),
                available,
                recordings_dir: if available {
                    "/media/1A2B-3C4D/LiveStage Recordings".into()
                } else {
                    "/data/livestage/recordings".into()
                },
            },
            volumes: {
                let mut volumes = vec![volume("internal", "sda", "lsdata")];
                if available {
                    volumes.push(volume("1A2B-3C4D", "sdb", "SHOW"));
                }
                volumes
            },
            disks: vec![
                Disk {
                    disk: "sda".into(),
                    model: Some("eMMC".into()),
                    size_bytes: 32_000_000_000,
                    removable: false,
                    system: true,
                },
                Disk {
                    disk: "sdb".into(),
                    model: Some("Stick".into()),
                    size_bytes: 64_000_000_000,
                    removable: true,
                    system: false,
                },
            ],
        }
    }

    fn internal() -> State {
        let mut state = on_stick(true);
        state.target = Target {
            id: "internal".into(),
            label: "Internal".into(),
            available: true,
            recordings_dir: "/data/livestage/recordings".into(),
        };
        state
    }

    /// Answers every request with `state` (a `use` moves the target) and
    /// remembers what it was asked.
    struct Fake {
        state: Arc<Mutex<State>>,
        asked: Arc<Mutex<Vec<Request>>>,
    }

    impl Transport for Fake {
        fn call(&mut self, request: &Request) -> Result<Reply, CallError> {
            self.asked.lock().unwrap().push(request.clone());
            let mut state = self.state.lock().unwrap();
            if let Request::Use { id } = request {
                *state = if id == "internal" {
                    internal()
                } else {
                    on_stick(true)
                };
            }
            Ok(Reply {
                ok: true,
                error: None,
                storage: state.clone(),
            })
        }
    }

    fn fake(state: State) -> (Storage, Receiver<Done>, Arc<Mutex<Vec<Request>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::channel();
        let storage = Storage::with_transport(
            Fake {
                state: Arc::new(Mutex::new(state)),
                asked: asked.clone(),
            },
            Duration::from_secs(3600),
            Box::new(move |done| {
                let _ = tx.send(done);
            }),
        );
        (storage, rx, asked)
    }

    fn next(rx: &Receiver<Done>) -> Done {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the worker answers")
    }

    fn web(client: ClientId) -> Origin {
        Origin {
            client: Some(client),
            request: Some(json!(7)),
        }
    }

    #[test]
    fn nothing_moves_the_disk_a_take_is_written_to() {
        let state = on_stick(true);
        let use_internal = Request::Use {
            id: "internal".into(),
        };
        assert!(refusal(&use_internal, Some(&state), true).is_some());
        assert!(refusal(&use_internal, Some(&state), false).is_none());

        let eject_target = Request::Eject {
            id: "1A2B-3C4D".into(),
        };
        assert!(refusal(&eject_target, Some(&state), true).is_some());
        assert!(refusal(&eject_target, Some(&state), false).is_none());
        // Another stick may go while recording.
        let eject_other = Request::Eject {
            id: "9999-0000".into(),
        };
        assert!(refusal(&eject_other, Some(&state), true).is_none());

        let format_target = Request::Format {
            disk: "sdb".into(),
            label: "LIVESTAGE".into(),
        };
        assert!(refusal(&format_target, Some(&state), true).is_some());
        assert!(refusal(&format_target, Some(&state), false).is_none());
        let format_other = Request::Format {
            disk: "sdc".into(),
            label: "LIVESTAGE".into(),
        };
        assert!(refusal(&format_other, Some(&state), true).is_none());
        // The system disk never.
        let format_system = Request::Format {
            disk: "sda".into(),
            label: "LIVESTAGE".into(),
        };
        assert!(refusal(&format_system, Some(&state), false).is_some());
        // Recording with nothing known: nothing risky.
        assert!(refusal(&eject_other, None, true).is_some());
    }

    #[test]
    fn a_refused_request_never_reaches_the_service() {
        let (mut storage, rx, asked) = fake(on_stick(true));
        let _ = storage.take(next(&rx)); // the first list
        let refused = storage.ask(
            &json!({"cmd": "storage", "op": "use", "volume": "internal"}),
            true,
            web(1),
        );
        assert!(refused.unwrap_err().contains("Stop recording"));
        let refused = storage.ask(
            &json!({"cmd": "storage", "op": "format", "disk": "sdb"}),
            true,
            web(1),
        );
        assert!(refused.is_err());
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(*asked.lock().unwrap(), vec![Request::List]);
    }

    #[test]
    fn requests_are_checked_before_they_are_sent() {
        let (mut storage, rx, _) = fake(internal());
        // Before the first list: not known yet.
        assert!(
            storage
                .ask(&json!({"op": "use", "volume": "1A2B-3C4D"}), false, web(1))
                .is_err()
        );
        let _ = storage.take(next(&rx));
        for bad in [
            json!({"op": "use"}),
            json!({"op": "mount", "volume": "x"}),
            json!({"op": "format", "disk": "sdb", "label": "TWELVE CHARS"}),
        ] {
            assert!(storage.ask(&bad, false, web(1)).is_err(), "{bad}");
        }
    }

    #[test]
    fn use_switches_the_folder_when_not_recording() {
        let (mut storage, rx, asked) = fake(internal());
        let _ = storage.take(next(&rx));
        let internal_dir = PathBuf::from("/data/livestage/recordings");
        assert_eq!(storage.folder_to_apply(Some(&internal_dir), false), None);

        storage
            .ask(
                &json!({"cmd": "storage", "op": "use", "volume": "1A2B-3C4D", "id": 7}),
                false,
                web(3),
            )
            .unwrap();
        // One change at a time, and no take until it is done.
        assert!(storage.blocks_recording().is_some());
        assert!(
            storage
                .ask(
                    &json!({"op": "eject", "volume": "1A2B-3C4D"}),
                    false,
                    web(4)
                )
                .is_err()
        );

        let (origin, reply) = storage.take(next(&rx)).expect("a reply is owed");
        assert_eq!(origin, web(3));
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["storage"]["target"]["id"], json!("1A2B-3C4D"));
        assert!(storage.blocks_recording().is_none());
        assert_eq!(
            asked.lock().unwrap().last(),
            Some(&Request::Use {
                id: "1A2B-3C4D".into()
            })
        );

        let stick = PathBuf::from("/media/1A2B-3C4D/LiveStage Recordings");
        assert_eq!(
            storage.folder_to_apply(Some(&internal_dir), false),
            Some(stick.clone())
        );
        // Never in the middle of a take.
        assert_eq!(storage.folder_to_apply(Some(&internal_dir), true), None);
        assert_eq!(storage.folder_to_apply(Some(&stick), false), None);
    }

    #[test]
    fn the_folder_falls_back_and_comes_back() {
        let stick = PathBuf::from("/media/1A2B-3C4D/LiveStage Recordings");
        let internal_dir = PathBuf::from("/data/livestage/recordings");
        let mut storage = Storage {
            jobs: None,
            state: None,
            unavailable: None,
            busy: None,
            sent: None,
        };
        let listed = |state: State| {
            Done::Listed(Ok(Reply {
                ok: true,
                error: None,
                storage: state,
            }))
        };
        // The stick is pulled: recordings go to the internal dir.
        storage.take(listed(on_stick(false)));
        assert_eq!(
            storage.folder_to_apply(Some(&stick), false),
            Some(internal_dir.clone())
        );
        // Not while a take is still going (it is the recorder's to finish).
        assert_eq!(storage.folder_to_apply(Some(&stick), true), None);
        // Back again.
        storage.take(listed(on_stick(true)));
        assert_eq!(
            storage.folder_to_apply(Some(&internal_dir), false),
            Some(stick.clone())
        );
        // The service goes away: the folder stays where it is.
        storage.take(Done::Listed(Err(CallError::Unavailable("gone".into()))));
        assert_eq!(storage.folder_to_apply(Some(&stick), false), None);
        assert_eq!(storage.message(&stick)["available"], json!(false));
        assert_eq!(storage.message(&stick)["reason"], json!("gone"));
    }

    #[test]
    fn the_message_goes_out_on_change_only() {
        let (mut storage, rx, _) = fake(internal());
        let folder = PathBuf::from("/data/livestage/recordings");
        assert!(storage.message_if_changed(&folder).is_some());
        assert!(storage.message_if_changed(&folder).is_none());
        let _ = storage.take(next(&rx));
        let message = storage
            .message_if_changed(&folder)
            .expect("the state arrived");
        assert_eq!(message["type"], json!("storage"));
        assert_eq!(message["available"], json!(true));
        assert_eq!(message["storage"]["target"]["id"], json!("internal"));
        assert_eq!(message["busy"], Value::Null);
        assert!(storage.message_if_changed(&folder).is_none());
    }

    #[test]
    fn a_failed_call_is_answered_and_frees_the_queue() {
        struct Broken;
        impl Transport for Broken {
            fn call(&mut self, request: &Request) -> Result<Reply, CallError> {
                match request {
                    Request::List => Ok(Reply {
                        ok: true,
                        error: None,
                        storage: internal(),
                    }),
                    _ => Err(CallError::Failed("did not answer in time".into())),
                }
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let mut storage = Storage::with_transport(
            Broken,
            Duration::from_secs(3600),
            Box::new(move |done| {
                let _ = tx.send(done);
            }),
        );
        let _ = storage.take(next(&rx));
        storage
            .ask(
                &json!({"op": "eject", "volume": "1A2B-3C4D"}),
                false,
                web(2),
            )
            .unwrap();
        let (_, reply) = storage.take(next(&rx)).unwrap();
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["error"], json!("did not answer in time"));
        // The state from the list is kept, and the next request may go.
        assert!(storage.state.is_some());
        assert!(storage.blocks_recording().is_none());
    }

    #[test]
    fn what_the_server_sends_is_the_contracts_lines() {
        let sent =
            |command: Value| serde_json::to_string(&parse_command(&command).unwrap()).unwrap();
        assert_eq!(
            sent(json!({"cmd": "storage", "op": "use", "volume": "internal", "id": 4})),
            r#"{"op":"use","id":"internal"}"#
        );
        assert_eq!(
            sent(json!({"cmd": "storage", "op": "eject", "volume": "1A2B-3C4D"})),
            r#"{"op":"eject","id":"1A2B-3C4D"}"#
        );
        assert_eq!(
            sent(json!({"cmd": "storage", "op": "format", "disk": "sdb", "label": " "})),
            r#"{"op":"format","disk":"sdb","label":"LIVESTAGE"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::List).unwrap(),
            r#"{"op":"list"}"#
        );
        // The contract's reply, both ways.
        let state = r#"{"target":{"id":"internal","label":"Internal","available":true,
            "recordings_dir":"/data/livestage/recordings"},
            "volumes":[{"id":"internal","label":"lsdata","fs":"exfat","device":"sda4","disk":"sda",
            "model":"SanDisk Ultra","size_bytes":64023257088,"free_bytes":61000000000,
            "mounted":"rw","mount_path":"/data","supported":true,"ejected":false}],
            "disks":[{"disk":"sdb","model":"SanDisk Ultra","size_bytes":64023257088,
            "removable":true,"system":false}]}"#;
        for text in [
            format!(r#"{{"ok":true,"storage":{state}}}"#),
            format!(r#"{{"ok":false,"error":"That drive is not plugged in.","storage":{state}}}"#),
        ] {
            let reply: Reply = serde_json::from_str(&text).unwrap();
            let again: Value = serde_json::to_value(&reply).unwrap();
            assert_eq!(again, serde_json::from_str::<Value>(&text).unwrap());
        }
    }

    /// The real transport against a fake service on a socket in a temp dir.
    #[cfg(unix)]
    #[test]
    fn the_unix_socket_speaks_line_json() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!("livestage-storage-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("storage.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let service = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                seen.push(serde_json::from_str::<Request>(&line).unwrap());
                let reply = Reply {
                    ok: seen.len() == 1,
                    error: (seen.len() == 2).then(|| "That disk is busy.".to_string()),
                    storage: internal(),
                };
                let mut text = serde_json::to_string(&reply).unwrap();
                text.push('\n');
                (&stream).write_all(text.as_bytes()).unwrap();
            }
            seen
        });

        let mut socket = unix::Socket::new(&path);
        let listed = socket.call(&Request::List).unwrap();
        assert!(listed.ok);
        assert_eq!(listed.storage.target.id, "internal");
        let eject = Request::Eject {
            id: "1A2B-3C4D".into(),
        };
        let refused = socket.call(&eject).unwrap();
        assert_eq!(refused.error.as_deref(), Some("That disk is busy."));
        assert_eq!(service.join().unwrap(), vec![Request::List, eject]);

        // No service: not available, not an error to show as a failure.
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            socket.call(&Request::List),
            Err(CallError::Unavailable(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
