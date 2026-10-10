//! Who may do what: the users file, PIN log-in, tokens, the lock, and the
//! permission table every web, OSC and MIDI command is checked against.
//!
//! With no users the server is in **open mode**, as before users existed:
//! every web client is an admin and needs no log-in. The first user added
//! must be an admin; from then on a web client sees nothing but `auth`
//! messages until it logs in (or resumes a token). stdin is always an admin.
//!
//! PINs are kept as PBKDF2-HMAC-SHA256 hashes with a random salt, checked
//! in constant time, and never sent back. Failed log-ins and unlocks are
//! rate limited per peer address and per user name. Tokens live in this
//! process only (a restart asks for the PIN again) and lapse after 12 hours
//! unused; a token a connected client holds counts as in use.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use livestage_engine::{Id, StripRef, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::web::ClientId;

/// A token unused this long is forgotten.
pub const TOKEN_IDLE: Duration = Duration::from_secs(12 * 3600);
/// Failures counted over this window …
const FAIL_WINDOW: Duration = Duration::from_secs(5 * 60);
/// … this many of them …
const FAIL_LIMIT: usize = 5;
/// … refuse further tries for this long after the last one.
const FAIL_WAIT: Duration = Duration::from_secs(30);
/// PBKDF2 rounds for a new PIN hash. A PIN is short, so the rate limit is
/// what really protects it; the rounds keep a copied users file from giving
/// the PINs away at a glance, while a check stays well under a tenth of a
/// second on the appliance (the command loop waits for it).
pub const PIN_ROUNDS: u32 = 60_000;
const PIN_MIN: usize = 4;
const PIN_MAX: usize = 32;
const NAME_MAX: usize = 32;

const WRONG_LOGIN: &str = "wrong name or PIN";
const WRONG_PIN: &str = "wrong PIN";
const TOO_MANY: &str = "too many attempts \u{2014} wait 30 s";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Engineer,
    Musician,
    Viewer,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Engineer => "engineer",
            Role::Musician => "musician",
            Role::Viewer => "viewer",
        }
    }
}

/// A personal mix a musician may change: an aux bus or a matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum MixRef {
    Bus(Id),
    Matrix(Id),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub role: Role,
    /// `pbkdf2-sha256$<rounds>$<salt b64>$<hash b64>`.
    pub pin: String,
    #[serde(default)]
    pub mixes: Vec<MixRef>,
}

impl User {
    /// As clients see a user: never the hash.
    fn view(&self) -> Value {
        json!({"name": self.name, "role": self.role, "mixes": self.mixes})
    }
}

#[derive(Default, Serialize, Deserialize)]
struct UsersFile {
    #[serde(default)]
    users: Vec<User>,
}

struct Token {
    /// `None`: an open-mode lock's token.
    user: Option<String>,
    locked: bool,
    /// Open mode: the PIN the lock was set with (hashed).
    lock_pin: Option<String>,
    last_used: Instant,
}

struct Client {
    peer: Option<IpAddr>,
    token: Option<String>,
    /// The `auth` message this client was last sent.
    sent: Option<Value>,
    /// Whether it was last let in (sent the session and the rest).
    admitted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Peer(IpAddr),
    Client(ClientId),
    Name(String),
}

/// Who a command comes from, as far as permissions go.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub role: Role,
    pub mixes: Vec<MixRef>,
    pub locked: bool,
}

impl Principal {
    /// OSC and MIDI: an engineer.
    pub fn remote() -> Self {
        Self {
            role: Role::Engineer,
            mixes: Vec::new(),
            locked: false,
        }
    }
}

/// An `auth`-family command's answer; `changed`: what some client may see
/// changed (call [`Users::refresh`]).
pub struct Outcome {
    pub reply: Value,
    pub changed: bool,
}

impl Outcome {
    fn reply(reply: Value) -> Self {
        Self {
            reply,
            changed: false,
        }
    }

    fn changed(reply: Value) -> Self {
        Self {
            reply,
            changed: true,
        }
    }

    fn error(error: impl Into<String>) -> Self {
        Self::reply(json!({"ok": false, "error": error.into()}))
    }
}

/// What a client is to be sent after a change: its new `auth` message (when
/// it differs), and whether it was just let in (send it everything) or just
/// shut out (stop sending).
pub struct Refresh {
    pub client: ClientId,
    pub auth: Option<Value>,
    pub admitted: Option<bool>,
}

pub struct Users {
    path: Option<PathBuf>,
    users: Vec<User>,
    tokens: HashMap<String, Token>,
    clients: HashMap<ClientId, Client>,
    failures: HashMap<Key, Vec<Instant>>,
    rounds: u32,
}

impl Users {
    /// The users file at `path` (none there: open mode), or in memory only.
    /// A file that cannot be read is an error: falling back to open mode
    /// would hand the mixer to anyone.
    pub fn open(path: Option<PathBuf>) -> Result<Self, String> {
        let mut users = Self::in_memory(PIN_ROUNDS);
        if let Some(path) = &path {
            if path.exists() {
                let text = std::fs::read_to_string(path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                let file: UsersFile = serde_json::from_str(&text)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                users.users = file.users;
            }
        }
        users.path = path;
        Ok(users)
    }

    pub fn in_memory(rounds: u32) -> Self {
        Self {
            path: None,
            users: Vec::new(),
            tokens: HashMap::new(),
            clients: HashMap::new(),
            failures: HashMap::new(),
            rounds,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn open_mode(&self) -> bool {
        self.users.is_empty()
    }

    pub fn count(&self) -> usize {
        self.users.len()
    }

    fn user(&self, name: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|u| u.name.eq_ignore_ascii_case(name))
    }

    pub fn join(&mut self, client: ClientId, peer: Option<IpAddr>) {
        self.clients.insert(
            client,
            Client {
                peer,
                token: None,
                sent: None,
                admitted: false,
            },
        );
    }

    pub fn leave(&mut self, client: ClientId) {
        self.clients.remove(&client);
    }

    /// Who `client` is now: `None` when it has to log in first.
    pub fn principal(&self, client: ClientId) -> Option<Principal> {
        let token = self
            .clients
            .get(&client)
            .and_then(|c| c.token.as_ref())
            .and_then(|t| self.tokens.get(t));
        let locked = token.is_some_and(|t| t.locked);
        if self.open_mode() {
            return Some(Principal {
                role: Role::Admin,
                mixes: Vec::new(),
                locked,
            });
        }
        let user = self.user(token?.user.as_deref()?)?;
        Some(Principal {
            role: user.role,
            mixes: user.mixes.clone(),
            locked,
        })
    }

    /// Whether `client` is sent the session, the meters and the rest.
    pub fn admitted(&self, client: ClientId) -> bool {
        self.open_mode() || self.principal(client).is_some()
    }

    fn client_user(&self, client: ClientId) -> Option<&User> {
        let token = self.clients.get(&client)?.token.as_ref()?;
        self.user(self.tokens.get(token)?.user.as_deref()?)
    }

    /// `{"type":"auth","mode","users","user","locked"}` as `client` sees it.
    pub fn auth_message(&self, client: ClientId) -> Value {
        let user = if self.open_mode() {
            None
        } else {
            self.client_user(client)
        };
        json!({
            "type": "auth",
            "mode": if self.open_mode() { "open" } else { "users" },
            "users": self
                .users
                .iter()
                .map(|u| json!({"name": u.name, "role": u.role}))
                .collect::<Vec<_>>(),
            "user": user.map(User::view),
            "locked": self.principal(client).is_some_and(|p| p.locked),
        })
    }

    /// Every client whose `auth` message or admission moved since last
    /// looked at.
    pub fn refresh(&mut self) -> Vec<Refresh> {
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        let mut out = Vec::new();
        for id in ids {
            let message = self.auth_message(id);
            let admitted = self.admitted(id);
            let Some(client) = self.clients.get_mut(&id) else {
                continue;
            };
            let auth = (client.sent.as_ref() != Some(&message)).then(|| message.clone());
            let moved = (client.admitted != admitted).then_some(admitted);
            client.sent = Some(message);
            client.admitted = admitted;
            if auth.is_some() || moved.is_some() {
                out.push(Refresh {
                    client: id,
                    auth,
                    admitted: moved,
                });
            }
        }
        out.sort_by_key(|r| r.client);
        out
    }

    /// A command from `client` used its token.
    pub fn touch(&mut self, client: ClientId, now: Instant) {
        if let Some(token) = self
            .clients
            .get(&client)
            .and_then(|c| c.token.as_ref())
            .and_then(|t| self.tokens.get_mut(t))
        {
            token.last_used = now;
        }
    }

    /// Forget tokens unused for [`TOKEN_IDLE`] (a connected client's token
    /// is in use). Whether any went.
    pub fn expire(&mut self, now: Instant) -> bool {
        for client in self.clients.values() {
            if let Some(token) = client.token.as_ref().and_then(|t| self.tokens.get_mut(t)) {
                token.last_used = now;
            }
        }
        let before = self.tokens.len();
        self.tokens
            .retain(|_, t| now.saturating_duration_since(t.last_used) < TOKEN_IDLE);
        self.tokens.len() != before
    }

    /// Check a command against `client`'s rights: `Err` is the refusal to
    /// send back. `cmd` is not one of the `auth` family (see [`command`]).
    pub fn check(&self, client: ClientId, cmd: &str, value: &Value) -> Result<(), Value> {
        match self.principal(client) {
            Some(principal) => permit(&principal, cmd, value),
            None => Err(json!({"ok": false, "error": "log in first", "denied": true})),
        }
    }

    // ----- the auth family --------------------------------------------------

    /// `login`, `resume`, `logout`, `auth`, `lock`, `unlock`, `users`,
    /// `user_add`, `user_set`, `user_remove`: answered here (`None` for any
    /// other command). `client` is `None` for stdin.
    pub fn command(
        &mut self,
        client: Option<ClientId>,
        cmd: &str,
        value: &Value,
        now: Instant,
    ) -> Option<Outcome> {
        if !FAMILY.contains(&cmd) {
            return None;
        }
        // Before logging in: only log in.
        if let Some(client) = client {
            if !self.admitted(client) && !matches!(cmd, "login" | "resume" | "auth") {
                return Some(Outcome::reply(
                    json!({"ok": false, "error": "log in first", "denied": true}),
                ));
            }
        }
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
        let web_only = |cmd: &str| Outcome::error(format!("{cmd} is for web clients"));
        Some(match (cmd, client) {
            ("auth", Some(client)) => {
                let mut reply = self.auth_message(client);
                reply["ok"] = json!(true);
                if let Some(fields) = reply.as_object_mut() {
                    fields.remove("type");
                }
                Outcome::reply(reply)
            }
            ("auth", None) => Outcome::reply(
                json!({"ok": true, "mode": if self.open_mode() { "open" } else { "users" }}),
            ),
            ("login", Some(client)) => self.login(client, text("name"), text("pin"), now),
            ("resume", Some(client)) => self.resume(client, text("token"), now),
            ("logout", Some(client)) => self.logout(client),
            ("lock", Some(client)) => {
                self.lock(client, value.get("pin").and_then(Value::as_str), now)
            }
            ("unlock", Some(client)) => self.unlock(client, text("pin"), now),
            ("login" | "resume" | "logout" | "lock" | "unlock", None) => web_only(cmd),
            ("users", _) => match self.admin(client) {
                Ok(()) => Outcome::reply(json!({
                    "ok": true,
                    "users": self.users.iter().map(User::view).collect::<Vec<_>>(),
                })),
                Err(refusal) => Outcome::reply(refusal),
            },
            ("user_add", _) => self.user_add(client, value, now),
            ("user_set", _) => self.user_set(client, value, now),
            ("user_remove", _) => match self.admin(client).and_then(|()| self.unlocked(client)) {
                Ok(()) => self.user_remove(text("name")),
                Err(refusal) => Outcome::reply(refusal),
            },
            _ => return None,
        })
    }

    /// stdin, or a web client whose role is admin.
    fn admin(&self, client: Option<ClientId>) -> Result<(), Value> {
        let Some(client) = client else {
            return Ok(());
        };
        match self.principal(client) {
            Some(p) if p.role == Role::Admin => Ok(()),
            Some(p) => Err(denied(p.role)),
            None => Err(json!({"ok": false, "error": "log in first", "denied": true})),
        }
    }

    fn unlocked(&self, client: Option<ClientId>) -> Result<(), Value> {
        match client.and_then(|c| self.principal(c)) {
            Some(p) if p.locked => Err(json!({"ok": false, "error": "locked"})),
            _ => Ok(()),
        }
    }

    fn keys(&self, client: ClientId, name: Option<&str>) -> Vec<Key> {
        let mut keys = vec![match self.clients.get(&client).and_then(|c| c.peer) {
            Some(peer) => Key::Peer(peer),
            None => Key::Client(client),
        }];
        if let Some(name) = name {
            keys.push(Key::Name(name.to_lowercase()));
        }
        keys
    }

    fn blocked(&mut self, keys: &[Key], now: Instant) -> bool {
        keys.iter().any(|key| {
            let Some(times) = self.failures.get_mut(key) else {
                return false;
            };
            times.retain(|t| now.saturating_duration_since(*t) < FAIL_WINDOW);
            times.len() >= FAIL_LIMIT
                && times
                    .last()
                    .is_some_and(|last| now.saturating_duration_since(*last) < FAIL_WAIT)
        })
    }

    fn failed(&mut self, keys: &[Key], now: Instant) {
        for key in keys {
            self.failures.entry(key.clone()).or_default().push(now);
        }
    }

    fn succeeded(&mut self, keys: &[Key]) {
        for key in keys {
            self.failures.remove(key);
        }
    }

    fn new_token(&mut self, client: ClientId, user: Option<String>, now: Instant) -> String {
        let token = hex(&random_bytes::<16>());
        self.tokens.insert(
            token.clone(),
            Token {
                user,
                locked: false,
                lock_pin: None,
                last_used: now,
            },
        );
        if let Some(c) = self.clients.get_mut(&client) {
            c.token = Some(token.clone());
        }
        token
    }

    fn login(&mut self, client: ClientId, name: &str, pin: &str, now: Instant) -> Outcome {
        if self.open_mode() {
            return Outcome::error("there are no users: everyone has full control");
        }
        let keys = self.keys(client, Some(name));
        if self.blocked(&keys, now) {
            return Outcome::error(TOO_MANY);
        }
        let matched = self
            .user(name)
            .filter(|u| verify_pin(pin, &u.pin))
            .map(|u| (u.name.clone(), u.view()));
        let Some((name, view)) = matched else {
            self.failed(&keys, now);
            return Outcome::error(WRONG_LOGIN);
        };
        self.succeeded(&keys);
        let token = self.new_token(client, Some(name), now);
        Outcome::changed(json!({"ok": true, "token": token, "user": view}))
    }

    fn resume(&mut self, client: ClientId, token: &str, now: Instant) -> Outcome {
        let valid = self.tokens.get_mut(token).is_some_and(|t| {
            t.last_used = now;
            true
        });
        if !valid {
            return Outcome::error("this log-in has expired: log in again");
        }
        if let Some(c) = self.clients.get_mut(&client) {
            c.token = Some(token.to_string());
        }
        let user = self.client_user(client).map(User::view);
        if !self.open_mode() && user.is_none() {
            // An open-mode lock's token, from before there were users.
            if let Some(c) = self.clients.get_mut(&client) {
                c.token = None;
            }
            return Outcome::error("this log-in has expired: log in again");
        }
        let locked = self.principal(client).is_some_and(|p| p.locked);
        Outcome::changed(json!({"ok": true, "user": user, "locked": locked}))
    }

    fn logout(&mut self, client: ClientId) -> Outcome {
        let Some(token) = self.clients.get(&client).and_then(|c| c.token.clone()) else {
            return Outcome::reply(json!({"ok": true}));
        };
        // Open mode: the token is the lock; dropping it must not unlock.
        if self.open_mode() && self.tokens.get(&token).is_some_and(|t| t.locked) {
            return Outcome::error("locked");
        }
        self.tokens.remove(&token);
        for c in self.clients.values_mut() {
            if c.token.as_deref() == Some(&token) {
                c.token = None;
            }
        }
        Outcome::changed(json!({"ok": true}))
    }

    fn lock(&mut self, client: ClientId, pin: Option<&str>, now: Instant) -> Outcome {
        if self.open_mode() {
            let Some(pin) = pin else {
                return Outcome::error("choose a PIN to unlock with");
            };
            if let Err(error) = check_pin(pin) {
                return Outcome::error(error);
            }
            if self.principal(client).is_some_and(|p| p.locked) {
                return Outcome::error("already locked");
            }
            let hash = hash_pin(pin, self.rounds);
            let token = self.new_token(client, None, now);
            if let Some(t) = self.tokens.get_mut(&token) {
                t.locked = true;
                t.lock_pin = Some(hash);
            }
            return Outcome::changed(json!({"ok": true, "token": token}));
        }
        let Some(token) = self.clients.get(&client).and_then(|c| c.token.clone()) else {
            return Outcome::error("log in first");
        };
        match self.tokens.get_mut(&token) {
            Some(t) => t.locked = true,
            None => return Outcome::error("log in first"),
        }
        Outcome::changed(json!({"ok": true, "token": token}))
    }

    fn unlock(&mut self, client: ClientId, pin: &str, now: Instant) -> Outcome {
        let Some(token) = self.clients.get(&client).and_then(|c| c.token.clone()) else {
            return Outcome::reply(json!({"ok": true}));
        };
        let Some(t) = self.tokens.get(&token) else {
            return Outcome::reply(json!({"ok": true}));
        };
        if !t.locked {
            return Outcome::reply(json!({"ok": true}));
        }
        let (name, lock_pin) = (t.user.clone(), t.lock_pin.clone());
        let keys = self.keys(client, name.as_deref());
        if self.blocked(&keys, now) {
            return Outcome::error(TOO_MANY);
        }
        let right = match (&lock_pin, &name) {
            (Some(lock_pin), _) => verify_pin(pin, lock_pin),
            (None, Some(name)) => {
                self.user(name).is_some_and(|u| verify_pin(pin, &u.pin))
                    || self
                        .users
                        .iter()
                        .filter(|u| u.role == Role::Admin)
                        .any(|u| verify_pin(pin, &u.pin))
            }
            (None, None) => false,
        };
        if !right {
            self.failed(&keys, now);
            return Outcome::error(WRONG_PIN);
        }
        self.succeeded(&keys);
        if name.is_none() {
            // Open mode: the token was only the lock.
            self.tokens.remove(&token);
            for c in self.clients.values_mut() {
                if c.token.as_deref() == Some(&token) {
                    c.token = None;
                }
            }
        } else if let Some(t) = self.tokens.get_mut(&token) {
            t.locked = false;
        }
        Outcome::changed(json!({"ok": true}))
    }

    fn user_add(&mut self, client: Option<ClientId>, value: &Value, now: Instant) -> Outcome {
        if let Err(refusal) = self.admin(client).and_then(|()| self.unlocked(client)) {
            return Outcome::reply(refusal);
        }
        #[derive(Deserialize)]
        struct Add {
            name: String,
            role: Role,
            pin: String,
            #[serde(default)]
            mixes: Vec<MixRef>,
        }
        let add: Add = match serde_json::from_value(value.clone()) {
            Ok(add) => add,
            Err(error) => return Outcome::error(error.to_string()),
        };
        let name = add.name.trim().to_string();
        if let Err(error) = self
            .check_name(&name, None)
            .and_then(|()| check_pin(&add.pin))
        {
            return Outcome::error(error);
        }
        let first = self.open_mode();
        if first && add.role != Role::Admin {
            return Outcome::error("the first user must be an admin");
        }
        let user = User {
            name: name.clone(),
            role: add.role,
            pin: hash_pin(&add.pin, self.rounds),
            mixes: if add.role == Role::Musician {
                add.mixes
            } else {
                Vec::new()
            },
        };
        let view = user.view();
        self.users.push(user);
        if let Err(error) = self.persist() {
            self.users.pop();
            return Outcome::error(error);
        }
        // Whoever made the first user is now logged in as it; open-mode
        // locks mean nothing any more.
        if first {
            self.tokens.clear();
            for c in self.clients.values_mut() {
                c.token = None;
            }
            if let Some(client) = client {
                let token = self.new_token(client, Some(name), now);
                return Outcome::changed(json!({"ok": true, "token": token, "user": view}));
            }
        }
        Outcome::changed(json!({"ok": true}))
    }

    fn user_set(&mut self, client: Option<ClientId>, value: &Value, now: Instant) -> Outcome {
        if let Err(refusal) = self.unlocked(client) {
            return Outcome::reply(refusal);
        }
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(index) = self
            .users
            .iter()
            .position(|u| u.name.eq_ignore_ascii_case(name))
        else {
            return Outcome::error(format!("no user {name}"));
        };
        let role = match value.get("role").filter(|v| !v.is_null()) {
            Some(role) => match serde_json::from_value::<Role>(role.clone()) {
                Ok(role) => Some(role),
                Err(error) => return Outcome::error(error.to_string()),
            },
            None => None,
        };
        let mixes = match value.get("mixes").filter(|v| !v.is_null()) {
            Some(mixes) => match serde_json::from_value::<Vec<MixRef>>(mixes.clone()) {
                Ok(mixes) => Some(mixes),
                Err(error) => return Outcome::error(error.to_string()),
            },
            None => None,
        };
        let pin = value.get("pin").and_then(Value::as_str);
        let new_name = value
            .get("new_name")
            .and_then(Value::as_str)
            .map(|n| n.trim().to_string());

        let is_admin = self.admin(client).is_ok();
        let own = client
            .and_then(|c| self.client_user(c))
            .is_some_and(|u| u.name == self.users[index].name);
        if !is_admin {
            // Anyone may change their own PIN, given the old one.
            let only_pin = role.is_none() && mixes.is_none() && new_name.is_none();
            let Some(client) = client.filter(|_| own && only_pin && pin.is_some()) else {
                let role = client
                    .and_then(|c| self.principal(c))
                    .map_or(Role::Viewer, |p| p.role);
                return Outcome::reply(denied(role));
            };
            let keys = self.keys(client, Some(name));
            if self.blocked(&keys, now) {
                return Outcome::error(TOO_MANY);
            }
            let old = value
                .get("old_pin")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !verify_pin(old, &self.users[index].pin) {
                self.failed(&keys, now);
                return Outcome::error(WRONG_PIN);
            }
            self.succeeded(&keys);
        }
        if let Some(pin) = pin {
            if let Err(error) = check_pin(pin) {
                return Outcome::error(error);
            }
        }
        if let Some(new_name) = &new_name {
            if let Err(error) = self.check_name(new_name, Some(index)) {
                return Outcome::error(error);
            }
        }
        if let Some(role) = role {
            if role != Role::Admin && self.users[index].role == Role::Admin && self.admins() == 1 {
                return Outcome::error("the last admin cannot be given another role");
            }
        }

        let before = self.users[index].clone();
        let user = &mut self.users[index];
        if let Some(role) = role {
            user.role = role;
        }
        if let Some(mixes) = mixes {
            user.mixes = mixes;
        }
        if user.role != Role::Musician {
            user.mixes.clear();
        }
        if let Some(pin) = pin {
            user.pin = hash_pin(pin, self.rounds);
        }
        if let Some(new_name) = &new_name {
            user.name = new_name.clone();
        }
        if let Err(error) = self.persist() {
            self.users[index] = before;
            return Outcome::error(error);
        }
        if let Some(new_name) = new_name {
            for token in self.tokens.values_mut() {
                if token
                    .user
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case(&before.name))
                {
                    token.user = Some(new_name.clone());
                }
            }
        }
        Outcome::changed(json!({"ok": true, "user": self.users[index].view()}))
    }

    fn user_remove(&mut self, name: &str) -> Outcome {
        let Some(index) = self
            .users
            .iter()
            .position(|u| u.name.eq_ignore_ascii_case(name))
        else {
            return Outcome::error(format!("no user {name}"));
        };
        if self.users[index].role == Role::Admin && self.admins() == 1 {
            return Outcome::error("the last admin cannot be removed");
        }
        let removed = self.users.remove(index);
        if let Err(error) = self.persist() {
            self.users.insert(index, removed);
            return Outcome::error(error);
        }
        // Logged out wherever they are.
        self.tokens
            .retain(|_, t| t.user.as_deref() != Some(removed.name.as_str()));
        Outcome::changed(json!({"ok": true}))
    }

    fn admins(&self) -> usize {
        self.users.iter().filter(|u| u.role == Role::Admin).count()
    }

    fn check_name(&self, name: &str, except: Option<usize>) -> Result<(), String> {
        if name.is_empty() || name.chars().count() > NAME_MAX {
            return Err(format!("a name is 1 to {NAME_MAX} characters"));
        }
        let taken = self
            .users
            .iter()
            .enumerate()
            .any(|(i, u)| Some(i) != except && u.name.eq_ignore_ascii_case(name));
        if taken {
            return Err(format!("there is already a user {name}"));
        }
        Ok(())
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let file = UsersFile {
            users: self.users.clone(),
        };
        let text = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
        write_atomic(path, &text)
    }
}

// ----- permissions -----------------------------------------------------------

/// The commands [`Users::command`] answers.
pub const FAMILY: &[&str] = &[
    "auth",
    "login",
    "resume",
    "logout",
    "lock",
    "unlock",
    "users",
    "user_add",
    "user_set",
    "user_remove",
];

/// Commands that change nothing: anyone logged in may send them, locked or
/// not. (`users` is in the auth family, admins only; `remote_settings` and
/// `midi_ports` read the remote setup, admins only.)
pub const READ_ONLY: &[&str] = &[
    "status",
    "session",
    "meters",
    "devices",
    "effects",
    "inserts",
    "installed",
    "library",
    "takes",
    "auth",
    // An editor's measurements, for this client only.
    "watch_insert",
    "remote_settings",
    "midi_ports",
];

/// What only an admin may do.
pub const ADMIN_ONLY: &[&str] = &[
    "storage",
    "set_audio",
    "remote_settings",
    "set_remote",
    "midi_ports",
    "midi_learn",
    "midi_learn_cancel",
    "users",
    "user_add",
    "user_remove",
];

/// `{"ok":false,"error":"not allowed (musician)","denied":true}`.
pub fn denied(role: Role) -> Value {
    json!({"ok": false, "error": format!("not allowed ({})", role.name()), "denied": true})
}

/// Whether `principal` may send `cmd` (with its fields in `value`).
pub fn permit(principal: &Principal, cmd: &str, value: &Value) -> Result<(), Value> {
    if ADMIN_ONLY.contains(&cmd) && principal.role != Role::Admin {
        return Err(denied(principal.role));
    }
    if READ_ONLY.contains(&cmd) {
        return Ok(());
    }
    if principal.locked {
        return Err(json!({"ok": false, "error": "locked"}));
    }
    let allowed = match principal.role {
        Role::Admin | Role::Engineer => true,
        Role::Musician => musician_may(&principal.mixes, cmd, value),
        Role::Viewer => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(denied(principal.role))
    }
}

/// A musician changes their own mixes only: the sends into their bus (level
/// and pan, not pre/post), their bus's or matrix's fader, mute and pan, and
/// their matrix's sources.
fn musician_may(mixes: &[MixRef], cmd: &str, value: &Value) -> bool {
    let id = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|v| Id::try_from(v).ok())
    };
    match cmd {
        "set_send" => {
            id("bus").is_some_and(|bus| mixes.contains(&MixRef::Bus(bus)))
                && value.get("pre_fader").is_none_or(Value::is_null)
        }
        "set_fader" | "set_mute" | "set_pan" => {
            match value
                .get("strip")
                .and_then(|s| serde_json::from_value::<StripRef>(s.clone()).ok())
            {
                Some(StripRef::Bus(id)) => mixes.contains(&MixRef::Bus(id)),
                Some(StripRef::Matrix(id)) => mixes.contains(&MixRef::Matrix(id)),
                _ => false,
            }
        }
        "set_matrix_send" => id("matrix").is_some_and(|m| mixes.contains(&MixRef::Matrix(m))),
        _ => false,
    }
}

// ----- PIN hashing ------------------------------------------------------------

fn check_pin(pin: &str) -> Result<(), String> {
    let n = pin.chars().count();
    if (PIN_MIN..=PIN_MAX).contains(&n) {
        Ok(())
    } else {
        Err(format!("a PIN is {PIN_MIN} to {PIN_MAX} characters"))
    }
}

pub fn hash_pin(pin: &str, rounds: u32) -> String {
    let salt = random_bytes::<16>();
    let mut hash = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pin.as_bytes(), &salt, rounds, &mut hash);
    format!(
        "pbkdf2-sha256${rounds}${}${}",
        base64_encode(&salt),
        base64_encode(&hash)
    )
}

/// Whether `pin` hashes to `stored`, compared in constant time. A stored
/// value that does not parse matches nothing.
pub fn verify_pin(pin: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let (Some("pbkdf2-sha256"), Some(rounds), Some(salt), Some(hash), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    let (Ok(rounds), Some(salt), Some(hash)) = (
        rounds.parse::<u32>(),
        base64_decode(salt),
        base64_decode(hash),
    ) else {
        return false;
    };
    if rounds == 0 || hash.is_empty() || hash.len() > 64 {
        return false;
    }
    let mut derived = vec![0u8; hash.len()];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pin.as_bytes(), &salt, rounds, &mut derived);
    constant_time_eq(&derived, &hash)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// `N` bytes from the operating system's random source.
fn random_bytes<const N: usize>() -> [u8; N] {
    use rand::TryRngCore;
    let mut bytes = [0u8; N];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .expect("the operating system's random source failed");
    bytes
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROUNDS: u32 = 16;

    fn users() -> Users {
        Users::in_memory(ROUNDS)
    }

    fn ok(outcome: &Outcome) -> bool {
        outcome.reply["ok"] == true
    }

    /// Ann (admin, 1111), Ed (engineer, 2222), Mo (musician on bus 5 and
    /// matrix 9, 3333), Vi (viewer, 4444); client 1 made Ann.
    fn band(now: Instant) -> Users {
        let mut u = users();
        u.join(1, Some("10.0.0.1".parse().unwrap()));
        let add = |u: &mut Users, v: Value| {
            let outcome = u.command(Some(1), "user_add", &v, now).unwrap();
            assert!(ok(&outcome), "{}", outcome.reply);
        };
        add(
            &mut u,
            json!({"name": "Ann", "role": "admin", "pin": "1111"}),
        );
        add(
            &mut u,
            json!({"name": "Ed", "role": "engineer", "pin": "2222"}),
        );
        add(
            &mut u,
            json!({"name": "Mo", "role": "musician", "pin": "3333",
                   "mixes": [{"kind": "bus", "id": 5}, {"kind": "matrix", "id": 9}]}),
        );
        add(
            &mut u,
            json!({"name": "Vi", "role": "viewer", "pin": "4444"}),
        );
        u
    }

    fn login(u: &mut Users, client: ClientId, name: &str, pin: &str, now: Instant) -> Outcome {
        u.command(
            Some(client),
            "login",
            &json!({"name": name, "pin": pin}),
            now,
        )
        .unwrap()
    }

    #[test]
    fn pins_hash_and_verify_and_never_match_garbage() {
        let stored = hash_pin("1234", ROUNDS);
        assert!(stored.starts_with("pbkdf2-sha256$16$"));
        assert!(verify_pin("1234", &stored));
        assert!(!verify_pin("1235", &stored));
        assert!(!verify_pin("1234", "1234"));
        assert!(!verify_pin("", "pbkdf2-sha256$0$AAAA$AAAA"));
        assert_ne!(stored, hash_pin("1234", ROUNDS), "salted");
        // Known PBKDF2-HMAC-SHA256 vector (RFC 7914 §11, 1 round).
        let mut out = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"passwd", b"salt", 1, &mut out);
        assert_eq!(hex(&out), "55ac046e56e3089fec1691c22544b605");
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(base64_decode(&base64_encode(bytes)).unwrap(), bytes);
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn open_mode_is_every_client_an_admin_until_the_first_user() {
        let now = Instant::now();
        let mut u = users();
        u.join(1, None);
        u.join(2, None);
        assert!(u.open_mode());
        assert_eq!(u.principal(2).unwrap().role, Role::Admin);
        assert!(u.check(2, "set_audio", &json!({})).is_ok());
        assert_eq!(u.auth_message(1)["mode"], "open");
        assert_eq!(u.auth_message(1)["user"], Value::Null);

        let first = u
            .command(
                Some(1),
                "user_add",
                &json!({"name": "Ed", "role": "engineer", "pin": "1234"}),
                now,
            )
            .unwrap();
        assert_eq!(first.reply["error"], "the first user must be an admin");
        let short = u
            .command(
                Some(1),
                "user_add",
                &json!({"name": "Ann", "role": "admin", "pin": "12"}),
                now,
            )
            .unwrap();
        assert!(!ok(&short));
        let first = u
            .command(
                Some(1),
                "user_add",
                &json!({"name": "Ann", "role": "admin", "pin": "1234"}),
                now,
            )
            .unwrap();
        assert!(ok(&first) && first.changed);
        assert_eq!(first.reply["token"].as_str().unwrap().len(), 32);
        assert_eq!(first.reply["user"]["name"], "Ann");
        // The maker is logged in as Ann; the other client is shut out.
        assert_eq!(u.principal(1).unwrap().role, Role::Admin);
        assert!(u.principal(2).is_none());
        let refreshed = u.refresh();
        let two = refreshed.iter().find(|r| r.client == 2).unwrap();
        assert_eq!(two.admitted, None, "never let in: nothing to take back");
        assert_eq!(two.auth.as_ref().unwrap()["mode"], "users");
        assert_eq!(two.auth.as_ref().unwrap()["users"][0]["name"], "Ann");
        assert!(two.auth.as_ref().unwrap()["users"][0].get("pin").is_none());
        assert_eq!(
            u.check(2, "status", &json!({})).unwrap_err()["error"],
            "log in first"
        );
    }

    #[test]
    fn a_users_file_from_before_users_existed_means_open_mode() {
        let dir = std::env::temp_dir().join(format!("livestage-users-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("users.json");
        let u = Users::open(Some(path.clone())).unwrap();
        assert!(u.open_mode());
        assert!(!path.exists(), "nothing written until a user is added");

        let mut u = Users::open(Some(path.clone())).unwrap();
        u.rounds = ROUNDS;
        u.join(1, None);
        let added = u.command(
            Some(1),
            "user_add",
            &json!({"name": "Ann", "role": "admin", "pin": "1234"}),
            Instant::now(),
        );
        assert!(ok(&added.unwrap()));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("1234"), "only the hash is stored");
        let back = Users::open(Some(path.clone())).unwrap();
        assert_eq!(back.count(), 1);
        assert!(verify_pin("1234", &back.users[0].pin));

        std::fs::write(&path, "{ not json").unwrap();
        assert!(
            Users::open(Some(path)).is_err(),
            "never open mode by accident"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn login_issues_a_token_that_resumes_and_logout_drops_it() {
        let now = Instant::now();
        let mut u = band(now);
        u.join(2, None);
        assert!(u.principal(2).is_none());
        let wrong = login(&mut u, 2, "Ed", "9999", now);
        assert_eq!(wrong.reply["error"], "wrong name or PIN");
        let nobody = login(&mut u, 2, "Nobody", "2222", now);
        assert_eq!(nobody.reply["error"], "wrong name or PIN", "the same text");
        let ed = login(&mut u, 2, "ed", "2222", now);
        assert!(ok(&ed));
        assert_eq!(ed.reply["user"]["role"], "engineer");
        let token = ed.reply["token"].as_str().unwrap().to_string();
        assert_eq!(u.principal(2).unwrap().role, Role::Engineer);

        // A reload: a new connection resumes.
        u.leave(2);
        u.join(3, None);
        assert!(u.principal(3).is_none());
        let resumed = u
            .command(Some(3), "resume", &json!({"token": token}), now)
            .unwrap();
        assert!(ok(&resumed), "{}", resumed.reply);
        assert_eq!(resumed.reply["user"]["name"], "Ed");
        assert_eq!(u.principal(3).unwrap().role, Role::Engineer);
        let bad = u
            .command(Some(3), "resume", &json!({"token": "00"}), now)
            .unwrap();
        assert!(!ok(&bad));

        assert!(ok(&u.command(Some(3), "logout", &json!({}), now).unwrap()));
        assert!(u.principal(3).is_none());
        u.join(4, None);
        let again = u
            .command(Some(4), "resume", &json!({"token": token}), now)
            .unwrap();
        assert!(!ok(&again), "a dropped token is gone");
    }

    #[test]
    fn tokens_lapse_after_twelve_idle_hours_unless_in_use() {
        let t0 = Instant::now();
        let mut u = band(t0);
        u.join(2, None);
        let token = login(&mut u, 2, "Vi", "4444", t0).reply["token"]
            .as_str()
            .unwrap()
            .to_string();
        // Connected: in use however long.
        let t1 = t0 + TOKEN_IDLE + Duration::from_secs(60);
        assert!(!u.expire(t1));
        u.leave(2);
        assert!(!u.expire(t1 + TOKEN_IDLE - Duration::from_secs(1)));
        assert!(u.expire(t1 + TOKEN_IDLE));
        u.join(3, None);
        let resumed = u
            .command(Some(3), "resume", &json!({"token": token}), t1 + TOKEN_IDLE)
            .unwrap();
        assert!(!ok(&resumed));
        assert!(u.principal(1).is_some(), "Ann's page stayed connected");
    }

    #[test]
    fn five_failures_wait_thirty_seconds_per_peer_and_per_name() {
        let t0 = Instant::now();
        let mut u = band(t0);
        u.join(2, Some("10.0.0.2".parse().unwrap()));
        for i in 0..5 {
            let o = login(&mut u, 2, "Ed", "0000", t0 + Duration::from_secs(i));
            assert_eq!(o.reply["error"], "wrong name or PIN");
        }
        let t = t0 + Duration::from_secs(5);
        let refused = login(&mut u, 2, "Ed", "2222", t);
        assert_eq!(refused.reply["error"], TOO_MANY, "even the right PIN");
        // Another name from the same peer: the peer is blocked.
        assert_eq!(login(&mut u, 2, "Vi", "4444", t).reply["error"], TOO_MANY);
        // Ed from another peer: the name is blocked.
        u.join(3, Some("10.0.0.3".parse().unwrap()));
        assert_eq!(login(&mut u, 3, "Ed", "2222", t).reply["error"], TOO_MANY);
        assert!(ok(&login(&mut u, 3, "Vi", "4444", t)));
        // 30 s after the last failure: let through.
        let later = t0 + Duration::from_secs(4 + 30);
        assert!(ok(&login(&mut u, 2, "Ed", "2222", later)));
        // A reconnect is no way around it: the peer address is the key.
        for i in 0..5 {
            login(&mut u, 2, "Mo", "0000", later + Duration::from_secs(i));
        }
        u.leave(2);
        u.join(4, Some("10.0.0.2".parse().unwrap()));
        assert_eq!(
            login(&mut u, 4, "Vi", "4444", later + Duration::from_secs(6)).reply["error"],
            TOO_MANY
        );
    }

    #[test]
    fn each_role_may_do_what_the_table_says() {
        let now = Instant::now();
        let mut u = band(now);
        for (client, name, pin) in [(2, "Ed", "2222"), (3, "Mo", "3333"), (4, "Vi", "4444")] {
            u.join(client, None);
            assert!(ok(&login(&mut u, client, name, pin, now)));
        }
        let fader = |strip: Value| json!({"cmd": "set_fader", "strip": strip, "db": -3});
        let channel = json!({"kind": "channel", "id": 1});
        let their_bus = json!({"kind": "bus", "id": 5});
        let other_bus = json!({"kind": "bus", "id": 6});
        let their_matrix = json!({"kind": "matrix", "id": 9});
        let allowed = |u: &Users, c, cmd: &str, v: &Value| u.check(c, cmd, v).is_ok();

        // Admin (client 1): everything.
        for cmd in [
            "set_audio",
            "storage",
            "set_remote",
            "midi_learn",
            "remote_settings",
        ] {
            assert!(allowed(&u, 1, cmd, &json!({})), "admin {cmd}");
        }
        assert!(ok(&u.command(Some(1), "users", &json!({}), now).unwrap()));

        // Engineer: the mix, not the system.
        for cmd in [
            "set_fader",
            "scene_recall",
            "undo",
            "save",
            "add_insert",
            "playback",
        ] {
            assert!(
                allowed(&u, 2, cmd, &fader(channel.clone())),
                "engineer {cmd}"
            );
        }
        for cmd in [
            "set_audio",
            "storage",
            "set_remote",
            "remote_settings",
            "midi_ports",
            "midi_learn",
        ] {
            let refusal = u.check(2, cmd, &json!({})).unwrap_err();
            assert_eq!(refusal["error"], "not allowed (engineer)", "engineer {cmd}");
            assert_eq!(refusal["denied"], true);
        }
        for cmd in ["users", "user_add", "user_remove"] {
            let o = u
                .command(Some(2), cmd, &json!({"name": "Vi"}), now)
                .unwrap();
            assert_eq!(o.reply["error"], "not allowed (engineer)", "{cmd}");
        }
        let o = u
            .command(
                Some(2),
                "user_set",
                &json!({"name": "Vi", "role": "admin"}),
                now,
            )
            .unwrap();
        assert_eq!(o.reply["denied"], true);

        // Musician: their own mixes only.
        assert!(allowed(&u, 3, "status", &json!({})));
        assert!(allowed(&u, 3, "session", &json!({})));
        assert!(allowed(&u, 3, "meters", &json!({})));
        assert!(allowed(&u, 3, "set_fader", &fader(their_bus.clone())));
        assert!(allowed(&u, 3, "set_fader", &fader(their_matrix)));
        assert!(allowed(
            &u,
            3,
            "set_mute",
            &json!({"strip": their_bus, "mute": true})
        ));
        assert!(allowed(
            &u,
            3,
            "set_pan",
            &json!({"strip": their_bus, "pan": 0.5})
        ));
        let send = |bus: u32, extra: Value| {
            let mut v = json!({"cmd": "set_send", "channel": 1, "bus": bus, "level_db": -6});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            v
        };
        assert!(allowed(&u, 3, "set_send", &send(5, json!({}))));
        assert!(allowed(
            &u,
            3,
            "set_send",
            &send(5, json!({"pan": -0.5, "pan_follow": false}))
        ));
        assert!(!allowed(
            &u,
            3,
            "set_send",
            &send(5, json!({"pre_fader": false}))
        ));
        assert!(!allowed(&u, 3, "set_send", &send(6, json!({}))));
        assert!(allowed(
            &u,
            3,
            "set_matrix_send",
            &json!({"matrix": 9, "source": {"kind": "master"}, "level_db": -3})
        ));
        assert!(!allowed(
            &u,
            3,
            "set_matrix_send",
            &json!({"matrix": 8, "source": {"kind": "master"}, "level_db": -3})
        ));
        for (cmd, v) in [
            ("set_fader", fader(channel.clone())),
            ("set_fader", fader(other_bus)),
            ("set_fader", fader(json!({"kind": "master"}))),
            ("set_solo", json!({"strip": their_bus, "solo": true})),
            ("scene_recall", json!({"id": 1})),
            ("undo", json!({})),
            ("save", json!({})),
            ("talk", json!({"active": true})),
            ("rename_strip", json!({"strip": their_bus, "name": "x"})),
        ] {
            let refusal = u.check(3, cmd, &v).unwrap_err();
            assert_eq!(
                refusal,
                json!({"ok": false, "error": "not allowed (musician)", "denied": true}),
                "musician {cmd}"
            );
        }

        // Viewer: reads only.
        assert!(allowed(&u, 4, "session", &json!({})));
        assert!(allowed(&u, 4, "library", &json!({})));
        assert!(allowed(&u, 4, "watch_insert", &json!({"insert": 3})));
        for cmd in ["set_fader", "library_save", "set_send", "start_recording"] {
            assert_eq!(
                u.check(4, cmd, &fader(their_bus.clone())).unwrap_err()["error"],
                "not allowed (viewer)"
            );
        }

        // OSC and MIDI act as an engineer.
        let remote = Principal::remote();
        assert!(permit(&remote, "set_fader", &fader(channel)).is_ok());
        assert!(permit(&remote, "set_remote", &json!({})).is_err());
        let admin = Principal {
            role: Role::Admin,
            mixes: Vec::new(),
            locked: false,
        };
        assert!(permit(&admin, "set_audio", &json!({})).is_ok());
    }

    #[test]
    fn anyone_may_change_their_own_pin_given_the_old_one() {
        let now = Instant::now();
        let mut u = band(now);
        u.join(3, None);
        assert!(ok(&login(&mut u, 3, "Mo", "3333", now)));
        let set = |u: &mut Users, v: Value| u.command(Some(3), "user_set", &v, now).unwrap();
        let wrong = set(
            &mut u,
            json!({"name": "Mo", "pin": "5555", "old_pin": "0000"}),
        );
        assert_eq!(wrong.reply["error"], "wrong PIN");
        let other = set(
            &mut u,
            json!({"name": "Vi", "pin": "5555", "old_pin": "4444"}),
        );
        assert_eq!(other.reply["denied"], true);
        let mixes = set(
            &mut u,
            json!({"name": "Mo", "mixes": [], "old_pin": "3333"}),
        );
        assert_eq!(mixes.reply["denied"], true);
        let changed = set(
            &mut u,
            json!({"name": "Mo", "pin": "5555", "old_pin": "3333"}),
        );
        assert!(ok(&changed), "{}", changed.reply);
        u.join(5, None);
        assert!(!ok(&login(&mut u, 5, "Mo", "3333", now)));
        assert!(ok(&login(&mut u, 5, "Mo", "5555", now)));
    }

    #[test]
    fn the_last_admin_stays_and_changes_reach_connected_clients() {
        let now = Instant::now();
        let mut u = band(now);
        u.join(3, None);
        assert!(ok(&login(&mut u, 3, "Mo", "3333", now)));
        u.refresh();

        let o = u
            .command(Some(1), "user_remove", &json!({"name": "Ann"}), now)
            .unwrap();
        assert_eq!(o.reply["error"], "the last admin cannot be removed");
        let o = u
            .command(
                Some(1),
                "user_set",
                &json!({"name": "Ann", "role": "engineer"}),
                now,
            )
            .unwrap();
        assert!(!ok(&o));

        // Mo's mixes change: Mo's client is told at once.
        let o = u
            .command(
                Some(1),
                "user_set",
                &json!({"name": "Mo", "mixes": [{"kind": "bus", "id": 6}]}),
                now,
            )
            .unwrap();
        assert!(ok(&o) && o.changed);
        let refreshed = u.refresh();
        let mo = refreshed.iter().find(|r| r.client == 3).unwrap();
        assert_eq!(
            mo.auth.as_ref().unwrap()["user"]["mixes"],
            json!([{"kind": "bus", "id": 6}])
        );
        assert!(
            u.check(3, "set_fader", &json!({"strip": {"kind": "bus", "id": 6}}))
                .is_ok()
        );
        assert!(
            u.check(3, "set_fader", &json!({"strip": {"kind": "bus", "id": 5}}))
                .is_err()
        );

        // Renamed, still logged in.
        let o = u
            .command(
                Some(1),
                "user_set",
                &json!({"name": "Mo", "new_name": "Moe"}),
                now,
            )
            .unwrap();
        assert!(ok(&o));
        assert_eq!(u.auth_message(3)["user"]["name"], "Moe");

        // Removed: logged out.
        let o = u
            .command(Some(1), "user_remove", &json!({"name": "Moe"}), now)
            .unwrap();
        assert!(ok(&o));
        assert!(u.principal(3).is_none());
        let mo = u.refresh().into_iter().find(|r| r.client == 3).unwrap();
        assert_eq!(mo.admitted, Some(false));
        assert_eq!(mo.auth.unwrap()["user"], Value::Null);

        // A second admin makes the first removable.
        let o = u
            .command(
                Some(1),
                "user_set",
                &json!({"name": "Ed", "role": "admin"}),
                now,
            )
            .unwrap();
        assert!(ok(&o));
        assert!(ok(&u
            .command(None, "user_remove", &json!({"name": "Ann"}), now)
            .unwrap()));
        assert!(u.principal(1).is_none(), "Ann's own client is logged out");
    }

    #[test]
    fn a_locked_client_reads_but_changes_nothing() {
        let now = Instant::now();
        let mut u = band(now);
        u.join(2, None);
        assert!(ok(&login(&mut u, 2, "Ed", "2222", now)));
        let o = u.command(Some(2), "lock", &json!({}), now).unwrap();
        assert!(ok(&o));
        let token = o.reply["token"].as_str().unwrap().to_string();
        assert!(u.check(2, "session", &json!({})).is_ok());
        assert!(u.check(2, "meters", &json!({})).is_ok());
        assert_eq!(
            u.check(2, "set_fader", &json!({})).unwrap_err(),
            json!({"ok": false, "error": "locked"})
        );
        assert_eq!(u.auth_message(2)["locked"], true);

        // A reload stays locked.
        u.leave(2);
        u.join(3, None);
        let resumed = u
            .command(Some(3), "resume", &json!({"token": token}), now)
            .unwrap();
        assert_eq!(resumed.reply["locked"], true);
        assert!(u.check(3, "set_fader", &json!({})).is_err());

        // Wrong PIN, then an admin's PIN unlocks.
        let o = u
            .command(Some(3), "unlock", &json!({"pin": "4444"}), now)
            .unwrap();
        assert_eq!(o.reply["error"], "wrong PIN");
        let o = u
            .command(Some(3), "unlock", &json!({"pin": "1111"}), now)
            .unwrap();
        assert!(ok(&o));
        assert!(u.check(3, "set_fader", &json!({})).is_ok());
        // The user's own PIN unlocks too; locked unlocks share the limit.
        u.command(Some(3), "lock", &json!({}), now).unwrap();
        for i in 0..5 {
            u.command(
                Some(3),
                "unlock",
                &json!({"pin": "0000"}),
                now + Duration::from_secs(i),
            );
        }
        let o = u
            .command(
                Some(3),
                "unlock",
                &json!({"pin": "2222"}),
                now + Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(o.reply["error"], TOO_MANY);
        let o = u
            .command(
                Some(3),
                "unlock",
                &json!({"pin": "2222"}),
                now + Duration::from_secs(40),
            )
            .unwrap();
        assert!(ok(&o));
    }

    #[test]
    fn open_mode_locks_with_a_pin_chosen_on_the_spot() {
        let now = Instant::now();
        let mut u = users();
        u.join(1, None);
        let o = u.command(Some(1), "lock", &json!({}), now).unwrap();
        assert!(!ok(&o), "a PIN is needed");
        let o = u
            .command(Some(1), "lock", &json!({"pin": "2468"}), now)
            .unwrap();
        assert!(ok(&o));
        let token = o.reply["token"].as_str().unwrap().to_string();
        assert_eq!(
            u.check(1, "set_fader", &json!({})).unwrap_err()["error"],
            "locked"
        );
        assert!(
            !ok(&u.command(Some(1), "logout", &json!({}), now).unwrap()),
            "no way out"
        );
        let o = u
            .command(
                Some(1),
                "user_add",
                &json!({"name": "A", "role": "admin", "pin": "1234"}),
                now,
            )
            .unwrap();
        assert_eq!(o.reply["error"], "locked");

        u.leave(1);
        u.join(2, None);
        assert!(
            u.check(2, "set_fader", &json!({})).is_ok(),
            "only that page is locked"
        );
        let o = u
            .command(Some(2), "resume", &json!({"token": token}), now)
            .unwrap();
        assert_eq!(o.reply["locked"], true);
        assert!(u.check(2, "set_fader", &json!({})).is_err());
        let o = u
            .command(Some(2), "unlock", &json!({"pin": "1357"}), now)
            .unwrap();
        assert_eq!(o.reply["error"], "wrong PIN");
        let o = u
            .command(Some(2), "unlock", &json!({"pin": "2468"}), now)
            .unwrap();
        assert!(ok(&o));
        assert!(u.check(2, "set_fader", &json!({})).is_ok());
    }

    #[test]
    fn stdin_is_an_admin_and_never_logs_in() {
        let now = Instant::now();
        let mut u = band(now);
        let o = u.command(None, "users", &json!({}), now).unwrap();
        let list = o.reply["users"].as_array().unwrap();
        assert_eq!(list.len(), 4);
        assert!(list.iter().all(|u| u.get("pin").is_none()));
        let o = u
            .command(None, "login", &json!({"name": "Ann", "pin": "1111"}), now)
            .unwrap();
        assert!(!ok(&o));
        assert!(
            u.command(None, "set_fader", &json!({}), now).is_none(),
            "not ours"
        );
    }
}
