//! The server with users: what a client that has not logged in is sent, the
//! gate in front of every web command, and remote commands acting as an
//! engineer.

use std::sync::mpsc::Receiver;

use super::*;

/// The engine as far as these tests need it.
#[derive(Default)]
struct Stub {
    session: Session,
    revision: u64,
    applied: Vec<Command>,
}

impl Mixer for Stub {
    fn session(&self) -> &Session {
        &self.session
    }
    fn revision(&self) -> u64 {
        self.revision
    }
    fn history(&self) -> livestage_engine::History {
        livestage_engine::History::default()
    }
    fn scene_differs(&self, _id: Id) -> Option<bool> {
        None
    }
    fn apply_with_note(&mut self, command: Command) -> Result<Option<String>, String> {
        self.applied.push(command);
        self.revision += 1;
        Ok(None)
    }
    fn save(&mut self, path: &Path, _timeout: Duration) -> Result<(), String> {
        self.session.save(path)
    }
}

fn options() -> Options {
    Options {
        session: None,
        host: None,
        input: None,
        output: None,
        rate: None,
        buffer: None,
        record_dir: None,
        http: None,
        list_devices: false,
        daemon: false,
        autosave: false,
        autosave_seconds: 0,
        library: None,
        users: None,
        remote: None,
    }
}

/// A server with `clients` connected (ids 1…), each with its inbox.
fn server(clients: u64) -> (Server, Vec<Receiver<String>>) {
    let options = options();
    let saves = Saves::new(None, Instant::now(), None);
    let mut server = Server {
        options,
        clients: Vec::new(),
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
    let mut inboxes = Vec::new();
    for id in 1..=clients {
        let (outbox, inbox) = std::sync::mpsc::sync_channel(web::OUTBOX);
        server.clients.push((id, outbox));
        server.users.join(id, None);
        inboxes.push(inbox);
    }
    server.refresh_auth_with(welcome_stub);
    (server, inboxes)
}

/// What `welcome` sends first, without an engine.
fn welcome_stub(_: &Server, outbox: &SyncSender<String>) {
    let _ = outbox.try_send(json!({"type": "hello"}).to_string());
}

fn types(inbox: &Receiver<String>) -> Vec<String> {
    inbox
        .try_iter()
        .map(|text| {
            let value: Value = serde_json::from_str(&text).unwrap();
            value["type"].as_str().unwrap_or_default().to_string()
        })
        .collect()
}

fn messages(inbox: &Receiver<String>) -> Vec<Value> {
    inbox
        .try_iter()
        .map(|text| serde_json::from_str(&text).unwrap())
        .collect()
}

/// `cmd` from web client `id` through the gate: the gate's reply, else
/// what the command path answers.
fn send(server: &mut Server, mixer: &mut Stub, id: ClientId, request: Value) -> Value {
    if let Some((reply, changed)) = server.gate(id, Some(&request)) {
        if changed {
            server.refresh_auth_with(welcome_stub);
        }
        return reply;
    }
    engine_command(mixer, server, request, Source::Web(id))
}

fn every_push(server: &mut Server, mixer: &Stub) {
    server.push_session_if_changed(mixer);
    server.push_history_if_changed(mixer);
    server.push_scene_state_if_changed(mixer);
    server.push_storage_if_changed(mixer);
    let library = server.library.message();
    server.broadcast(&library);
    server.broadcast(&json!({"type": "meters", "meters": []}));
    server.broadcast(&json!({"type": "status"}));
    server.broadcast(&saved_message(
        Path::new("show.json"),
        SystemTime::now(),
        true,
    ));
}

#[test]
fn open_mode_lets_every_client_in_as_before() {
    let (mut server, inboxes) = server(2);
    for inbox in &inboxes {
        let sent = messages(inbox);
        assert_eq!(sent[0]["type"], "auth");
        assert_eq!(sent[0]["mode"], "open");
        assert_eq!(sent[1]["type"], "hello", "then everything");
    }
    let mut mixer = Stub::default();
    every_push(&mut server, &mixer);
    for inbox in &inboxes {
        let got = types(inbox);
        for kind in [
            "session",
            "history",
            "scene_state",
            "library",
            "meters",
            "status",
            "saved",
        ] {
            assert!(got.contains(&kind.to_string()), "{kind}: {got:?}");
        }
    }
    let reply = send(
        &mut server,
        &mut mixer,
        2,
        json!({"cmd": "set_fader", "strip": {"kind": "master"}, "db": -3}),
    );
    assert_eq!(reply, json!({"ok": true}));
}

#[test]
fn a_client_not_logged_in_is_sent_auth_only() {
    let (mut server, inboxes) = server(2);
    let mut mixer = Stub::default();
    // Client 1 makes the first user (an admin) and is logged in as it.
    let reply = send(
        &mut server,
        &mut mixer,
        1,
        json!({"cmd": "user_add", "name": "Ann", "role": "admin", "pin": "1234"}),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    let reply = send(
        &mut server,
        &mut mixer,
        1,
        json!({"cmd": "user_add", "name": "Mo", "role": "musician", "pin": "5678",
               "mixes": [{"kind": "bus", "id": 5}]}),
    );
    assert_eq!(reply["ok"], true, "{reply}");
    for inbox in &inboxes {
        let _ = messages(inbox);
    }

    every_push(&mut server, &mixer);
    mixer.revision += 1;
    every_push(&mut server, &mixer);
    assert!(!types(&inboxes[0]).is_empty(), "Ann gets everything");
    assert!(types(&inboxes[1]).is_empty(), "the other page gets nothing");
    // Not even admin messages.
    server.send_to_admins(&json!({"type": "remote"}));
    assert_eq!(types(&inboxes[0]), ["remote"]);
    assert!(types(&inboxes[1]).is_empty());

    // It may only log in.
    for request in [
        json!({"cmd": "status"}),
        json!({"cmd": "session"}),
        json!({"cmd": "set_fader", "strip": {"kind": "master"}, "db": 0}),
        json!({"cmd": "storage", "op": "eject", "volume": "x"}),
        json!({"cmd": "library"}),
        json!({"cmd": "users"}),
        json!({"cmd": "lock"}),
        json!({"cmd": "remote_settings"}),
    ] {
        let reply = send(&mut server, &mut mixer, 2, request.clone());
        assert_eq!(reply["error"], "log in first", "{request}");
    }
    let reply = send(&mut server, &mut mixer, 2, json!({"cmd": "auth"}));
    assert_eq!(reply["mode"], "users");
    assert_eq!(reply["users"].as_array().unwrap().len(), 2);
    assert!(mixer.applied.is_empty());

    // Logged in as Mo: the page is sent everything, and Mo moves Mo's mix only.
    let reply = send(
        &mut server,
        &mut mixer,
        2,
        json!({"cmd": "login", "name": "Mo", "pin": "5678"}),
    );
    assert_eq!(reply["ok"], true);
    let sent = messages(&inboxes[1]);
    assert_eq!(sent[0]["type"], "auth");
    assert_eq!(sent[0]["user"]["name"], "Mo");
    assert_eq!(sent[0]["user"]["mixes"], json!([{"kind": "bus", "id": 5}]));
    assert_eq!(sent[1]["type"], "hello");
    mixer.revision += 1;
    every_push(&mut server, &mixer);
    assert!(types(&inboxes[1]).contains(&"session".to_string()));

    let reply = send(
        &mut server,
        &mut mixer,
        2,
        json!({"cmd": "set_fader", "strip": {"kind": "bus", "id": 5}, "db": -6}),
    );
    assert_eq!(reply, json!({"ok": true}));
    let reply = send(
        &mut server,
        &mut mixer,
        2,
        json!({"cmd": "set_fader", "strip": {"kind": "master"}, "db": -6}),
    );
    assert_eq!(
        reply,
        json!({"ok": false, "error": "not allowed (musician)", "denied": true})
    );
    assert_eq!(mixer.applied.len(), 1);

    // Logged out: nothing more.
    let reply = send(&mut server, &mut mixer, 2, json!({"cmd": "logout"}));
    assert_eq!(reply["ok"], true);
    let sent = messages(&inboxes[1]);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["user"], Value::Null);
    mixer.revision += 1;
    every_push(&mut server, &mixer);
    assert!(types(&inboxes[1]).is_empty());
}

#[test]
fn a_locked_page_still_gets_everything_but_changes_nothing() {
    let (mut server, inboxes) = server(1);
    let mut mixer = Stub::default();
    let reply = send(
        &mut server,
        &mut mixer,
        1,
        json!({"cmd": "lock", "pin": "2468"}),
    );
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["token"].as_str().unwrap().len(), 32);
    let _ = messages(&inboxes[0]);
    every_push(&mut server, &mixer);
    assert!(types(&inboxes[0]).contains(&"meters".to_string()));
    let reply = send(
        &mut server,
        &mut mixer,
        1,
        json!({"cmd": "set_fader", "strip": {"kind": "master"}, "db": -6}),
    );
    assert_eq!(reply, json!({"ok": false, "error": "locked"}));
    let reply = send(
        &mut server,
        &mut mixer,
        1,
        json!({"cmd": "unlock", "pin": "2468"}),
    );
    assert_eq!(reply["ok"], true);
    assert_eq!(messages(&inboxes[0])[0]["locked"], false);
    let reply = send(
        &mut server,
        &mut mixer,
        1,
        json!({"cmd": "set_fader", "strip": {"kind": "master"}, "db": -6}),
    );
    assert_eq!(reply, json!({"ok": true}));
}

#[test]
fn remote_commands_act_as_an_engineer_and_are_vetted_like_a_web_clients() {
    let (mut server, _inboxes) = server(0);
    let mut mixer = Stub::default();
    let fader = json!({"cmd": "set_fader", "strip": {"kind": "master"}, "db": -6});
    assert!(users::permit(&users::Principal::remote(), "set_fader", &fader).is_ok());
    let reply = engine_command(&mut mixer, &mut server, fader, Source::Remote);
    assert_eq!(reply, json!({"ok": true}));
    // A recorder folder from outside stays the session's.
    let settings = json!({"cmd": "set_record_settings", "settings": {"folder": "/elsewhere"}});
    let reply = engine_command(&mut mixer, &mut server, settings, Source::Remote);
    assert_eq!(reply["ok"], true);
    assert!(matches!(
        &mixer.applied[1],
        Command::SetRecordSettings { settings } if settings.folder.is_none()
    ));
    for cmd in ["set_audio", "set_remote", "storage", "user_add"] {
        assert!(
            users::permit(&users::Principal::remote(), cmd, &json!({})).is_err(),
            "{cmd}"
        );
    }
}
