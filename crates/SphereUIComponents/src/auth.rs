//! Account sign-in against the Futureboard account service, session
//! persistence, and profile extraction.
//!
//! Community-wide: a Futureboard account is not an entitlement. What a *license*
//! grants still lives behind the Professional Edition's own verified license
//! provider, which asks this module for a session when a customer activates
//! from their account instead of entering a key.
//!
//! The account service is the website's own API on the site origin
//! (`futureboard.studio`), which is also what the licensing service asks when a
//! customer activates from their account instead of a key. One identity, one
//! session table, one place a sign-in can be revoked.
//!
//! The base must match the service's own `OAUTH_CALLBACK_BASE`. The flow sets
//! its state cookie on the host that starts it, and the callback lands on
//! whichever host the service is configured to redirect to — split them across
//! two hostnames and the cookie is simply absent on the callback.
//!
//! Trust model and secrets:
//! - **No client secret and no API key ship in the binary.** Sign-in happens in
//!   the system browser against the account service; the app only ever receives
//!   an opaque session credential for the account that signed in.
//! - The browser hands that session back one of two ways, and the app decides
//!   which per sign-in. An installed Studio owns the `fbrd://` scheme, so the
//!   service redirects to `fbrd://auth/callback` and the OS delivers it (see
//!   [`crate::deeplink`]); anything else — a `cargo run`, a portable copy —
//!   uses a one-shot **loopback** listener the app binds *before* the browser
//!   is pointed anywhere, on a port the OS assigns. Either way the app
//!   generates a nonce, the service echoes it, and a callback that does not
//!   carry it is refused — so a stray page cannot plant someone else's session
//!   in the app.
//! - TLS is mandatory in release builds. A debug build may point at a local
//!   account service on loopback.
//!
//! The session credential is stored in the per-user app-data directory
//! (`session.json`), written atomically (a temporary file renamed over the
//! old one). On Windows the credential in it is sealed with DPAPI for the
//! signed-in Windows user, so another account on the machine cannot use it
//! even if it can read the file; on macOS and Linux the file is created
//! readable by its owner only (0600). A file an older build wrote — the
//! credential in clear, default permissions — is rewritten that way the
//! first time it is read.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// Futureboard account API base, e.g. `https://futureboard.studio`. Baked
/// at build time from `.env`/the build environment; absent in an unconfigured
/// build, which then renders the sign-in UI disabled rather than guessing.
const BAKED_AUTH_API_URL: Option<&str> = option_env!("FUTUREBOARD_AUTH_API_URL");

const SESSION_FILE: &str = "session.json";

/// Overall ceiling on a browser sign-in before the loopback gives up, so a user
/// who closes the browser never leaks the waiting thread.
const OAUTH_WAIT_SECS: u64 = 180;

/// Loopback connections served at once. A browser opens one for the redirect
/// and may open a speculative one or two beside it; past this, more are
/// closed unread.
const MAX_CALLBACK_CONNECTIONS: usize = 8;

/// The longest request head the loopback reads from one connection.
const MAX_CALLBACK_HEAD: usize = 16 * 1024;

/// How long the loopback waits between looking for a new connection and
/// looking for a connection's verdict.
const CALLBACK_POLL: Duration = Duration::from_millis(50);

/// How long writing the reply page, and then waiting for the browser to close
/// its side, may take.
const CALLBACK_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// Network timeout for a single auth HTTP call.
const HTTP_TIMEOUT_SECS: u64 = 20;

/// How long a stored session is trusted before it is re-confirmed with the
/// service. Sessions last far longer than this; the check is what notices a
/// sign-out performed elsewhere, so the app is not left claiming a dead one.
const SESSION_RECHECK_SECS: u64 = 12 * 60 * 60;

fn auth_debug_enabled() -> bool {
    std::env::var("FUTUREBOARD_AUTH_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn debug_log(message: &str) {
    if auth_debug_enabled() {
        eprintln!("[Auth] {message}");
    }
}

// ── Config ───────────────────────────────────────────────────────────────────

/// Resolve the account API base URL. Release builds use the baked value and
/// require TLS; debug builds may override it for local development.
pub fn api_base_url() -> Option<String> {
    #[cfg(debug_assertions)]
    {
        if let Some(url) = std::env::var("FUTUREBOARD_AUTH_API_URL")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty() && is_acceptable_endpoint(value))
        {
            return Some(url);
        }
    }
    BAKED_AUTH_API_URL
        .map(|url| url.trim().trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty() && is_acceptable_endpoint(url))
}

/// TLS is mandatory. Debug builds additionally accept plaintext loopback so a
/// local account service can be developed against; `cfg!` folds to `false` in
/// release, so a shipped build has no plaintext path at all.
fn is_acceptable_endpoint(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    cfg!(debug_assertions)
        && (url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost"))
}

/// Whether this build can offer sign-in at all.
pub fn auth_configured() -> bool {
    api_base_url().is_some()
}

// ── Models ───────────────────────────────────────────────────────────────────

/// The signed-in user's public profile, pulled from the account service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub email: Option<String>,
    pub username: Option<String>,
    pub avatar_url: Option<String>,
    /// Whether the account service has proven this address belongs to the user.
    /// Account-based licensing requires it, so the UI can explain the gap
    /// instead of surfacing a bare refusal.
    #[serde(default)]
    pub email_verified: bool,
}

/// A persisted session. The credential is module-private so callers can never
/// read it out of a profile; serde still (de)serializes it within this module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Opaque session id issued by the account service, sent as a bearer token.
    token: String,
    /// Unix seconds this session was last confirmed with the service.
    #[serde(default)]
    checked_at: u64,
    pub user: UserProfile,
}

/// OAuth identity providers the account service wires up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProvider {
    Google,
    Discord,
    GitHub,
}

impl OAuthProvider {
    /// Path segment used by the account service (`/auth/{provider}`).
    fn as_path(self) -> &'static str {
        match self {
            OAuthProvider::Google => "google",
            OAuthProvider::Discord => "discord",
            OAuthProvider::GitHub => "github",
        }
    }

    /// Label for the sign-in button.
    pub fn label(self) -> &'static str {
        match self {
            OAuthProvider::Google => "Continue with Google",
            OAuthProvider::Discord => "Continue with Discord",
            OAuthProvider::GitHub => "Continue with GitHub",
        }
    }
}

// ── In-memory session cache ──────────────────────────────────────────────────

fn cache() -> &'static RwLock<Option<Session>> {
    static CACHE: OnceLock<RwLock<Option<Session>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(None))
}

/// Load the persisted session into memory and re-confirm it in the background
/// when it has not been checked recently. Call once at startup. Never blocks on
/// network — a slow account service must not delay the DAW opening.
pub fn init_session() {
    if let Some(session) = load_persisted_session() {
        debug_log(&format!(
            "loaded stored session for {}",
            session.user.username.as_deref().unwrap_or("(user)")
        ));
        if let Ok(mut guard) = cache().write() {
            *guard = Some(session);
        }
        spawn_refresh_if_due();
    }
}

/// The current signed-in profile, or `None` when signed out. Reads memory only.
pub fn current_profile() -> Option<UserProfile> {
    cache().read().ok()?.as_ref().map(|s| s.user.clone())
}

/// The session credential for the signed-in account, for callers that need to
/// prove who this is to another Futureboard service — today, the licensing
/// service's account activation.
///
/// Public only because the account layer and the licensing layer now live in
/// different crates: an account is Community-wide, what a license grants is
/// not. This is a **live credential** — the one caller sends it straight to the
/// activation endpoint over TLS. Never log it, never persist a second copy, and
/// never hand it to anything outside a Futureboard service.
pub fn session_token() -> Option<String> {
    cache().read().ok()?.as_ref().map(|s| s.token.clone())
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .https_only(!cfg!(debug_assertions))
        // The account service answers a native sign-in with a redirect to the
        // loopback listener; that hop belongs to the browser, never to this
        // client, so nothing here should chase a Location header.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("could not initialize sign-in: {error}"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The account service's `/me`. Only the fields the app shows are declared, so
/// the service can grow without breaking this build.
#[derive(Deserialize)]
struct MeResponse {
    user: AccountUser,
}

#[derive(Deserialize)]
struct AccountUser {
    #[serde(default)]
    id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "avatarUrl")]
    avatar_url: Option<String>,
    #[serde(default, rename = "emailVerifiedAt")]
    email_verified_at: Option<String>,
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Extract the display profile from an account user. The display name falls back
/// to the email local-part, which is what an OAuth provider that shares no name
/// leaves behind.
fn profile_from_user(user: AccountUser) -> UserProfile {
    let email = non_empty(user.email.as_deref());
    let username = non_empty(user.name.as_deref()).or_else(|| {
        email
            .as_deref()
            .and_then(|email| email.split('@').next())
            .map(str::to_string)
    });
    UserProfile {
        id: user.id,
        email,
        username,
        avatar_url: non_empty(user.avatar_url.as_deref()),
        email_verified: non_empty(user.email_verified_at.as_deref()).is_some(),
    }
}

/// Ask the account service who a session belongs to. Doubles as the liveness
/// check: a session the service no longer knows answers 401 here.
fn fetch_profile(token: &str) -> Result<UserProfile, String> {
    let base = api_base_url().ok_or("sign-in is not configured for this build")?;
    let response = http_client()?
        .get(format!("{base}/me"))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .map_err(|error| format!("the account service could not be reached: {error}"))?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err("that sign-in is no longer valid".to_string());
    }
    if !status.is_success() {
        return Err(format!("the account service returned {status}"));
    }
    let payload = response
        .json::<MeResponse>()
        .map_err(|error| format!("the account service returned an invalid response: {error}"))?;
    Ok(profile_from_user(payload.user))
}

/// Persist and cache a freshly obtained session.
fn apply_session(token: String, user: UserProfile) -> Result<UserProfile, String> {
    let session = Session {
        token,
        checked_at: now_unix(),
        user: user.clone(),
    };
    store_persisted_session(&session)?;
    if let Ok(mut guard) = cache().write() {
        *guard = Some(session);
    }
    Ok(user)
}

// ── Browser sign-in via the loopback callback ────────────────────────────────

/// Sign in with `provider` and, on success, pull the profile.
///
/// Blocking: opens the system browser, runs a one-shot loopback HTTP listener to
/// catch the redirect, then confirms the handed-back session. Meant to run on a
/// background thread — the UI keeps it off the GPUI thread.
///
/// There is no password path. The account service authenticates people through
/// their identity provider only, so a password field here would be a control
/// that cannot work.
pub fn oauth_sign_in(provider: OAuthProvider) -> Result<UserProfile, String> {
    let base = api_base_url().ok_or("sign-in is not configured for this build")?;

    // The nonce is generated here, not by the service: it is what proves the
    // callback answers *this* attempt, so a page that guesses the port — or
    // any page at all, for the scheme — cannot hand the app a session
    // belonging to someone else.
    let nonce = random_token(24);

    // An installed Studio takes the URL scheme: nothing has to listen, the
    // browser tab can close before the app has read anything, and a firewall
    // that dislikes loopback listeners has nothing to dislike. The loopback
    // form is what every other copy gets, and what this falls back to.
    if crate::deeplink::scheme_registered() {
        let receiver = register_scheme_sign_in(&nonce);
        let authorize = format!(
            "{base}/auth/{provider}?native={scheme}&nonce={nonce}",
            provider = provider.as_path(),
            scheme = crate::deeplink::SCHEME,
            nonce = url_encode(&nonce),
        );
        open_in_browser(&authorize)?;
        let token = match receiver.recv_timeout(Duration::from_secs(OAUTH_WAIT_SECS)) {
            Ok(result) => result,
            Err(_) => {
                cancel_scheme_sign_in();
                Err("timed out waiting for the browser sign-in".to_string())
            }
        }?;
        let profile = fetch_profile(&token)?;
        return apply_session(token, profile);
    }

    // Bind the listener up front so the port is ours before the browser is
    // pointed at it, and let the OS pick it: a fixed port is one already-running
    // instance away from failing, and nothing about this port needs to be known
    // in advance — the account service is told it in the request.
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .map_err(|error| format!("could not open a local sign-in port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("could not read the local sign-in port: {error}"))?
        .port();

    let authorize = format!(
        "{base}/auth/{provider}?native={port}&nonce={nonce}",
        provider = provider.as_path(),
        nonce = url_encode(&nonce),
    );

    open_in_browser(&authorize)?;
    let token = wait_for_session(listener, &nonce, Duration::from_secs(OAUTH_WAIT_SECS))?;

    let profile = fetch_profile(&token)?;
    apply_session(token, profile)
}

// ── Browser sign-in via the fbrd:// scheme ──────────────────────────────────

/// The sign-in currently waiting for an `fbrd://auth/callback`, if any: the
/// nonce it minted and where to deliver the answer. One at a time, because
/// the dialog runs one at a time; a second attempt replaces the first, whose
/// receiver then reports the wait as abandoned.
struct PendingSchemeSignIn {
    nonce: String,
    reply: mpsc::Sender<Result<String, String>>,
}

fn pending_scheme_sign_in() -> &'static Mutex<Option<PendingSchemeSignIn>> {
    static PENDING: OnceLock<Mutex<Option<PendingSchemeSignIn>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(None))
}

fn register_scheme_sign_in(nonce: &str) -> mpsc::Receiver<Result<String, String>> {
    let (reply, receiver) = mpsc::channel();
    if let Ok(mut pending) = pending_scheme_sign_in().lock() {
        *pending = Some(PendingSchemeSignIn {
            nonce: nonce.to_string(),
            reply,
        });
    }
    receiver
}

fn cancel_scheme_sign_in() {
    if let Ok(mut pending) = pending_scheme_sign_in().lock() {
        *pending = None;
    }
}

/// Finish a sign-in that came back through the URL scheme.
///
/// Called by the shell with whatever `fbrd://auth/callback` carried. The
/// nonce check is the whole of the trust here: the scheme is reachable from
/// any web page, so a callback that does not carry the nonce Studio minted for
/// the attempt currently waiting is not a sign-in, whatever session it names.
/// Returns whether a waiting sign-in took it.
pub fn complete_scheme_sign_in(state: &str, session: &str, error: Option<&str>) -> bool {
    let Ok(mut pending) = pending_scheme_sign_in().lock() else {
        return false;
    };
    let Some(waiting) = pending.as_ref() else {
        debug_log("a scheme sign-in callback arrived with nothing waiting for it");
        return false;
    };
    if !constant_time_eq(state.as_bytes(), waiting.nonce.as_bytes()) {
        debug_log("a scheme sign-in callback failed verification");
        return false;
    }
    let Some(waiting) = pending.take() else {
        return false;
    };
    let result = match error {
        Some(error) => Err(format!("sign-in was refused: {error}")),
        None if session.is_empty() => {
            Err("the sign-in response did not include a session".to_string())
        }
        None => Ok(session.to_string()),
    };
    let _ = waiting.reply.send(result);
    true
}

/// One-shot loopback capture of the `?fbsession=` (or `?error=`) redirect.
/// Verifies the `fbstate` nonce, ignores unrelated requests (e.g. favicon), and
/// gives up at `deadline` whatever the connections are doing.
///
/// Each connection is read on its own short-lived thread, so one that never
/// sends anything — a browser's speculative preconnect — cannot hold up the
/// redirect arriving on another, nor the deadline. Connections still open
/// when this returns are shut down, which ends their threads.
fn wait_for_session(
    listener: TcpListener,
    expected_state: &str,
    deadline: Duration,
) -> Result<String, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("local sign-in listener failed: {error}"))?;
    let until = Instant::now() + deadline;
    let (verdicts, verdict) = mpsc::channel::<Result<String, String>>();
    let serving = Arc::new(AtomicUsize::new(0));
    let mut open = OpenCallbacks(Vec::new());
    loop {
        if let Ok(result) = verdict.try_recv() {
            return result;
        }
        let Some(left) = until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            return Err("timed out waiting for the browser sign-in".to_string());
        };
        match listener.accept() {
            Ok((stream, _)) => {
                if serving.load(Ordering::Acquire) >= MAX_CALLBACK_CONNECTIONS {
                    continue; // dropped unread
                }
                let answered = Arc::new(AtomicBool::new(false));
                if let Ok(handle) = stream.try_clone() {
                    open.0.push((handle, answered.clone()));
                }
                serving.fetch_add(1, Ordering::AcqRel);
                let expected = expected_state.to_string();
                let verdicts = verdicts.clone();
                let done = serving.clone();
                let spawned = std::thread::Builder::new()
                    .name("futureboard-auth-callback".into())
                    .spawn(move || {
                        if let Some(result) = serve_callback(stream, &expected, until, &answered) {
                            let _ = verdicts.send(result);
                        }
                        done.fetch_sub(1, Ordering::AcqRel);
                    });
                if spawned.is_err() {
                    serving.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(result) = verdict.recv_timeout(left.min(CALLBACK_POLL)) {
                    return result;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("local sign-in listener failed: {error}")),
        }
    }
}

/// The loopback connections accepted so far, each with whether it has been
/// answered. When the wait ends, the ones never answered are shut down so a
/// reader still blocked on one returns at once; an answered one is left to
/// close itself, so its reply page is not cut off.
struct OpenCallbacks(Vec<(TcpStream, Arc<AtomicBool>)>);

impl Drop for OpenCallbacks {
    fn drop(&mut self) {
        for (stream, answered) in &self.0 {
            if !answered.load(Ordering::Acquire) {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

/// Serve one loopback connection: read its request head (all of it, however
/// it arrives, until `until`), answer it, and close it so the browser keeps
/// the page. `None` means "not the callback, keep waiting"; `Some` is the
/// terminal result (session credential or error).
fn serve_callback(
    mut stream: TcpStream,
    expected_state: &str,
    until: Instant,
    answered: &AtomicBool,
) -> Option<Result<String, String>> {
    // Accepted from a non-blocking listener, the stream is non-blocking too on
    // Windows and macOS, so a read before the browser has sent its request
    // failed at once and the redirect was dropped. Linux does not pass the
    // mode on, and there a read with no timeout waited forever on a
    // connection that never sends. Blocking reads with a deadline, here.
    stream.set_nonblocking(false).ok()?;
    let _ = stream.set_write_timeout(Some(CALLBACK_REPLY_TIMEOUT));
    let head = read_request_head(&mut stream, until)?;
    let head = String::from_utf8_lossy(&head);
    let request_line = head.lines().next().unwrap_or_default();
    let (result, message) = callback_verdict(request_line, expected_state);
    answered.store(true, Ordering::Release);
    let _ = write_browser_response(&mut stream, message);
    // Close only after the browser has the whole reply: our write side first,
    // then wait (briefly) for it to close its own. Dropping a socket with
    // unread input sends a reset, which can cost the browser the page.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(CALLBACK_REPLY_TIMEOUT));
    let mut sink = [0u8; 512];
    while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
    result
}

/// Read until the blank line that ends a request head. `None` when the
/// connection closes, stalls past `until`, or sends more than
/// [`MAX_CALLBACK_HEAD`] without ending its head.
fn read_request_head(stream: &mut TcpStream, until: Instant) -> Option<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let left = until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())?;
        stream.set_read_timeout(Some(left)).ok()?;
        let read = match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        let searched_from = head.len().saturating_sub(3);
        head.extend_from_slice(&chunk[..read]);
        if head[searched_from..].windows(4).any(|w| w == b"\r\n\r\n") {
            return Some(head);
        }
        if head.len() > MAX_CALLBACK_HEAD {
            return None;
        }
    }
}

/// What one loopback request line means for the sign-in, and the page to
/// answer it with. `None`: not the callback (a favicon, a stray hit, or a
/// request whose `fbstate` is not this attempt's nonce).
fn callback_verdict(
    request_line: &str,
    expected_state: &str,
) -> (Option<Result<String, String>>, &'static str) {
    // "GET /path?query HTTP/1.1"
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);

    let has_auth = params.iter().any(|(k, _)| k == "fbsession" || k == "error");
    if !has_auth {
        return (None, "Waiting for sign-in…");
    }

    let state_ok = params
        .iter()
        .find(|(k, _)| k == "fbstate")
        .map(|(_, v)| constant_time_eq(v.as_bytes(), expected_state.as_bytes()))
        .unwrap_or(false);
    if !state_ok {
        // Not this attempt's callback: anything on the machine can reach the
        // port, so an unverified request is answered and ignored rather than
        // allowed to end the sign-in (the scheme path does the same).
        return (None, "Sign-in could not be verified.");
    }

    if let Some((_, error)) = params.iter().find(|(k, _)| k == "error") {
        return (
            Some(Err(format!("sign-in was refused: {error}"))),
            "Sign-in was cancelled.",
        );
    }

    match params.into_iter().find(|(k, _)| k == "fbsession") {
        Some((_, session)) if !session.is_empty() => (
            Some(Ok(session)),
            "Signed in to Futureboard Studio. You can close this tab.",
        ),
        _ => (
            Some(Err(
                "the sign-in response did not include a session".to_string()
            )),
            "Sign-in did not complete.",
        ),
    }
}

fn write_browser_response(stream: &mut TcpStream, message: &str) -> std::io::Result<()> {
    // Plain text, no reflected input beyond our own fixed strings.
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Futureboard Studio</title>\
         <body style=\"font-family:system-ui;background:#16181d;color:#e6e6e6;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

// ── Session liveness ─────────────────────────────────────────────────────────

fn recheck_is_due(session: &Session) -> bool {
    now_unix().saturating_sub(session.checked_at) >= SESSION_RECHECK_SECS
}

/// Re-confirm the stored session with the account service and refresh the cached
/// profile (a renamed account or a new avatar lands here).
fn recheck_session() -> Result<UserProfile, String> {
    let token = session_token().ok_or("there is no session to check")?;
    let profile = fetch_profile(&token)?;
    apply_session(token, profile)
}

/// Re-confirm the stored session in the background when it is stale. Returns
/// immediately; a signed-out or recently-checked session does nothing.
///
/// Only an answer from the service — "this session is not valid" — signs the app
/// out. An unreachable service leaves the session alone, because a musician
/// offline is not a musician signed out.
pub fn spawn_refresh_if_due() {
    let due = cache()
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(recheck_is_due))
        .unwrap_or(false);
    if !due || !auth_configured() {
        return;
    }

    std::thread::Builder::new()
        .name("futureboard-auth-check".into())
        .spawn(|| match recheck_session() {
            Ok(_) => debug_log("session confirmed"),
            Err(error) => {
                if error.contains("no longer valid") {
                    debug_log(&format!("{error}; signing out"));
                    clear_local_session();
                } else {
                    debug_log(&format!("session check deferred: {error}"));
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| debug_log(&format!("could not start session check: {error}")));
}

// ── Sign out ─────────────────────────────────────────────────────────────────

/// Sign out: clear the local session immediately, then best-effort end it on the
/// service so the same credential cannot be used again. Local sign-out never
/// fails.
pub fn sign_out() {
    let token = session_token();
    clear_local_session();

    let (Some(base), Some(token)) = (api_base_url(), token) else {
        return;
    };
    std::thread::Builder::new()
        .name("futureboard-auth-logout".into())
        .spawn(move || {
            let result = http_client().and_then(|client| {
                client
                    .post(format!("{base}/auth/logout"))
                    .bearer_auth(&token)
                    .send()
                    .map_err(|error| format!("logout call failed: {error}"))
            });
            if let Err(error) = result {
                debug_log(&error);
            }
        })
        .ok();
}

fn clear_local_session() {
    if let Ok(mut guard) = cache().write() {
        *guard = None;
    }
    let path = session_path();
    if path.exists() {
        if let Err(error) = std::fs::remove_file(&path) {
            debug_log(&format!("could not remove stored session: {error}"));
        }
    }
}

// ── Persistence ──────────────────────────────────────────────────────────────

fn app_data_dir() -> PathBuf {
    crate::paths::FutureboardPaths::resolve().app_data
}

fn session_path() -> PathBuf {
    app_data_dir().join(SESSION_FILE)
}

/// `session.json` as written. The credential is in exactly one of the two
/// fields: sealed on Windows, in clear elsewhere (in a file only its owner
/// can read). A file from an older build has it in clear on every platform.
#[derive(Serialize, Deserialize)]
struct StoredSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    /// DPAPI-sealed for the Windows user who signed in, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sealed_token: Option<String>,
    #[serde(default)]
    checked_at: u64,
    user: UserProfile,
}

fn store_persisted_session(session: &Session) -> Result<(), String> {
    store_session_at(&session_path(), session)
}

fn load_persisted_session() -> Option<Session> {
    let path = session_path();
    let (session, outdated) = load_session_at(&path)?;
    if outdated {
        // Written by an older build: the credential in clear, or a file
        // others may read. Rewrite it the way this build stores it.
        match store_session_at(&path, &session) {
            Ok(()) => debug_log("stored session moved to protected storage"),
            Err(error) => debug_log(&format!("could not protect the stored session: {error}")),
        }
    }
    Some(session)
}

fn store_session_at(path: &Path, session: &Session) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create the session directory: {error}"))?;
    }
    let (token, sealed_token) = if cfg!(windows) {
        let sealed = sealed::seal(session.token.as_bytes())?;
        (None, Some(URL_SAFE_NO_PAD.encode(sealed)))
    } else {
        (Some(session.token.clone()), None)
    };
    let stored = StoredSession {
        token,
        sealed_token,
        checked_at: session.checked_at,
        user: session.user.clone(),
    };
    let json = serde_json::to_vec_pretty(&stored)
        .map_err(|error| format!("could not encode the session: {error}"))?;
    write_private_atomically(path, &json)
        .map_err(|error| format!("could not save the session: {error}"))
}

/// The stored session, and whether its file needs rewriting (an older
/// build's: the credential in clear on Windows, or readable by others
/// elsewhere).
fn load_session_at(path: &Path) -> Option<(Session, bool)> {
    let bytes = std::fs::read(path).ok()?;
    let stored = match serde_json::from_slice::<StoredSession>(&bytes) {
        Ok(stored) => stored,
        Err(error) => {
            // A session written by an older build (the Supabase shape) lands
            // here. Dropping it costs one sign-in and is the only honest read of
            // a credential this build cannot use.
            debug_log(&format!("stored session is unreadable: {error}"));
            return None;
        }
    };
    let (token, outdated) = match (stored.sealed_token, stored.token) {
        (Some(sealed), _) => {
            // Sealed for another Windows user, or on another machine: not a
            // credential this user can use.
            let opened = URL_SAFE_NO_PAD
                .decode(sealed.as_bytes())
                .map_err(|error| error.to_string())
                .and_then(|bytes| sealed::unseal(&bytes))
                .and_then(|bytes| String::from_utf8(bytes).map_err(|error| error.to_string()));
            match opened {
                Ok(token) => (token, false),
                Err(error) => {
                    debug_log(&format!("stored session could not be opened: {error}"));
                    return None;
                }
            }
        }
        (None, Some(token)) => (token, cfg!(windows) || readable_by_others(path)),
        (None, None) => return None,
    };
    if token.is_empty() {
        return None;
    }
    Some((
        Session {
            token,
            checked_at: stored.checked_at,
            user: stored.user,
        },
        outdated,
    ))
}

/// Write `bytes` to `path` through a temporary file in the same folder that is
/// renamed over it, so a crash mid-write leaves the old file whole. On Unix the
/// file is created owner-read/write only (0600).
fn write_private_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| SESSION_FILE.to_string());
    let temp = path.with_file_name(format!("{file_name}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temp);
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Whether anyone but the owner may read the file at `path`.
#[cfg(unix)]
fn readable_by_others(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o077 != 0)
        .unwrap_or(false)
}

/// Windows keeps the credential sealed instead; the file's own access is the
/// per-user app-data folder's.
#[cfg(not(unix))]
fn readable_by_others(_path: &Path) -> bool {
    false
}

/// DPAPI: the credential sealed with a key only the signed-in Windows user
/// (on this machine) can open.
#[cfg(windows)]
mod sealed {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB, String> {
        Ok(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(data.len()).map_err(|_| "the session is too large")?,
            pbData: data.as_ptr().cast_mut(),
        })
    }

    /// Copy out and free what DPAPI handed back.
    fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if output.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: on success DPAPI returns `cbData` bytes at `pbData`, allocated
        // with LocalAlloc; they are copied before being freed, exactly once.
        unsafe {
            let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
            bytes
        }
    }

    pub(super) fn seal(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` points at `data`, alive for the call; DPAPI only
        // reads it and writes `output`.
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|error| format!("could not protect the session: {error}"))?;
        Ok(take(output))
    }

    pub(super) fn unseal(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(data)?;
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: as in `seal`.
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|error| format!("could not open the stored session: {error}"))?;
        Ok(take(output))
    }
}

/// Elsewhere the file's own permissions protect it; nothing is sealed.
#[cfg(not(windows))]
mod sealed {
    pub(super) fn seal(data: &[u8]) -> Result<Vec<u8>, String> {
        Ok(data.to_vec())
    }

    pub(super) fn unseal(data: &[u8]) -> Result<Vec<u8>, String> {
        Ok(data.to_vec())
    }
}

// ── Small helpers ────────────────────────────────────────────────────────────

/// A URL-safe random token from OS entropy, ~`bytes` of randomness.
fn random_token(bytes: usize) -> String {
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

fn open_in_browser(url: &str) -> Result<(), String> {
    let mut command = browser_command();
    command
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not open your browser for sign-in: {error}"))
}

#[cfg(target_os = "windows")]
fn browser_command() -> std::process::Command {
    // rundll32 hands the URL straight to the default handler, avoiding cmd's
    // `start` quoting quirks with `&` in OAuth query strings.
    let mut command = std::process::Command::new("rundll32.exe");
    command.arg("url.dll,FileProtocolHandler");
    command
}

#[cfg(target_os = "macos")]
fn browser_command() -> std::process::Command {
    std::process::Command::new("open")
}

#[cfg(target_os = "linux")]
fn browser_command() -> std::process::Command {
    std::process::Command::new("xdg-open")
}

/// Percent-encode a value for use inside a URL query. Conservative: only
/// unreserved characters pass through unescaped.
fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Parse `a=b&c=d` into pairs, percent-decoding each side.
pub(crate) fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(part), String::new()),
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hi = (bytes[index + 1] as char).to_digit(16);
                let lo = (bytes[index + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push(((hi << 4) | lo) as u8);
                    index += 3;
                    continue;
                }
                out.push(b'%');
                index += 1;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Length-independent compare for the callback nonce.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_prefers_the_account_name_then_the_email_localpart() {
        let profile = profile_from_user(AccountUser {
            id: "abc".to_string(),
            email: Some("jane@example.com".to_string()),
            name: Some("  Jane Doe ".to_string()),
            avatar_url: Some("https://cdn/x.png".to_string()),
            email_verified_at: Some("2026-01-01T00:00:00Z".to_string()),
        });
        assert_eq!(profile.username.as_deref(), Some("Jane Doe"));
        assert_eq!(profile.avatar_url.as_deref(), Some("https://cdn/x.png"));
        assert_eq!(profile.email.as_deref(), Some("jane@example.com"));
        assert!(profile.email_verified);
    }

    #[test]
    fn profile_falls_back_to_the_email_localpart() {
        let profile = profile_from_user(AccountUser {
            id: "abc".to_string(),
            email: Some("bob@example.com".to_string()),
            name: None,
            avatar_url: None,
            email_verified_at: None,
        });
        assert_eq!(profile.username.as_deref(), Some("bob"));
        assert!(profile.avatar_url.is_none());
        // Account licensing needs a proven address, so an unproven one must not
        // read as verified.
        assert!(!profile.email_verified);
    }

    #[test]
    fn query_parsing_and_percent_decoding() {
        let params = parse_query("fbsession=ab%2Fcd&fbstate=xyz&error=");
        assert_eq!(params[0], ("fbsession".to_string(), "ab/cd".to_string()));
        assert_eq!(params[1], ("fbstate".to_string(), "xyz".to_string()));
        assert_eq!(params[2], ("error".to_string(), String::new()));
    }

    #[test]
    fn url_encode_escapes_reserved_characters() {
        assert_eq!(
            url_encode("http://127.0.0.1:8788/?fbstate=a b"),
            "http%3A%2F%2F127.0.0.1%3A8788%2F%3Ffbstate%3Da%20b"
        );
    }

    /// The nonce is the only thing standing between the loopback listener and a
    /// session someone else handed it.
    #[test]
    fn the_callback_nonce_must_match() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert_ne!(random_token(24), random_token(24));
    }

    /// The scheme is reachable from any web page, so the nonce is the only
    /// thing that makes an `fbrd://auth/callback` a sign-in rather than an
    /// injection.
    #[test]
    fn a_scheme_callback_completes_only_the_sign_in_that_minted_its_nonce() {
        assert!(!complete_scheme_sign_in("anything", "sess", None));

        let receiver = register_scheme_sign_in("the-real-nonce-value");
        assert!(!complete_scheme_sign_in(
            "guessed",
            "attacker-session",
            None
        ));
        assert!(
            receiver.try_recv().is_err(),
            "a wrong nonce must deliver nothing"
        );

        assert!(complete_scheme_sign_in(
            "the-real-nonce-value",
            "sess-1",
            None
        ));
        assert_eq!(
            receiver.try_recv().expect("delivered"),
            Ok("sess-1".to_string())
        );
        // Consumed: a replay finds nothing waiting.
        assert!(!complete_scheme_sign_in(
            "the-real-nonce-value",
            "sess-1",
            None
        ));

        let receiver = register_scheme_sign_in("second-nonce-value-here");
        assert!(complete_scheme_sign_in(
            "second-nonce-value-here",
            "",
            Some("access_denied")
        ));
        assert!(receiver.try_recv().expect("delivered").is_err());
    }

    fn callback_listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    /// A connection that never sends (a browser's speculative preconnect) must
    /// not hold up the redirect on another, and a request that arrives in
    /// pieces is still read whole and answered with the page.
    #[test]
    fn the_loopback_takes_the_redirect_past_an_idle_connection_and_a_split_request() {
        let (listener, address) = callback_listener();
        let started = Instant::now();
        let waiter = std::thread::spawn(move || {
            wait_for_session(listener, "nonce-123", Duration::from_secs(20))
        });

        let _idle = TcpStream::connect(address).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let mut browser = TcpStream::connect(address).unwrap();
        browser.write_all(b"GET /?fbsession=sess-1&fbst").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        browser
            .write_all(b"ate=nonce-123 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reply = String::new();
        let _ = browser.read_to_string(&mut reply);

        assert_eq!(waiter.join().unwrap(), Ok("sess-1".to_string()));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(reply.starts_with("HTTP/1.1 200 OK"), "{reply}");
        assert!(reply.contains("Signed in"), "{reply}");
    }

    /// An idle connection must not keep the sign-in waiting past its deadline.
    #[test]
    fn the_loopback_gives_up_at_its_deadline_with_a_connection_open() {
        let (listener, address) = callback_listener();
        let started = Instant::now();
        let waiter = std::thread::spawn(move || {
            wait_for_session(listener, "nonce", Duration::from_millis(400))
        });
        let _idle = TcpStream::connect(address).unwrap();
        let result = waiter.join().unwrap();
        assert!(result.unwrap_err().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_stray_request_is_answered_and_the_wait_goes_on() {
        let (verdict, _) = callback_verdict("GET /favicon.ico HTTP/1.1", "n");
        assert!(verdict.is_none());
        // A wrong or missing nonce is not this sign-in's answer: ignored, not
        // a reason to abort.
        let (verdict, _) = callback_verdict("GET /?fbsession=s&fbstate=bad HTTP/1.1", "n");
        assert!(verdict.is_none());
        let (verdict, _) = callback_verdict("GET /?error=access_denied HTTP/1.1", "n");
        assert!(verdict.is_none());
        let (verdict, _) = callback_verdict("GET /?error=x&fbstate=bad HTTP/1.1", "n");
        assert!(verdict.is_none());
        // The real nonce still decides, for a session or a refusal.
        let (verdict, _) = callback_verdict("GET /?fbsession=s&fbstate=n HTTP/1.1", "n");
        assert_eq!(verdict, Some(Ok("s".to_string())));
        let (verdict, _) = callback_verdict("GET /?error=denied&fbstate=n HTTP/1.1", "n");
        assert!(verdict.unwrap().is_err());
    }

    /// A stray request carrying a session but the wrong nonce must not cancel
    /// the sign-in in progress: the real redirect that follows still lands.
    #[test]
    fn a_forged_callback_does_not_cancel_the_sign_in() {
        let (listener, address) = callback_listener();
        let waiter = std::thread::spawn(move || {
            wait_for_session(listener, "nonce-xyz", Duration::from_secs(20))
        });

        let mut stray = TcpStream::connect(address).unwrap();
        stray
            .write_all(b"GET /?fbsession=attacker&fbstate=guess HTTP/1.1\r\n\r\n")
            .unwrap();
        stray
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reply = String::new();
        let _ = stray.read_to_string(&mut reply);
        assert!(reply.contains("could not be verified"), "{reply}");

        let mut browser = TcpStream::connect(address).unwrap();
        browser
            .write_all(b"GET /?fbsession=sess-ok&fbstate=nonce-xyz HTTP/1.1\r\n\r\n")
            .unwrap();
        browser
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reply = String::new();
        let _ = browser.read_to_string(&mut reply);

        assert_eq!(waiter.join().unwrap(), Ok("sess-ok".to_string()));
        assert!(reply.contains("Signed in"), "{reply}");
    }

    fn session_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fb-auth-test-{name}-{}-{}",
            std::process::id(),
            random_token(6)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn profile() -> UserProfile {
        UserProfile {
            id: "u1".to_string(),
            email: None,
            username: Some("jane".to_string()),
            avatar_url: None,
            email_verified: false,
        }
    }

    /// The stored credential is sealed on Windows and in an owner-only file
    /// elsewhere, and reads back as it was.
    #[test]
    fn a_stored_session_is_protected_and_reads_back() {
        let dir = session_dir("store");
        let path = dir.join(SESSION_FILE);
        let session = Session {
            token: "live-credential-value".to_string(),
            checked_at: 42,
            user: profile(),
        };
        store_session_at(&path, &session).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        if cfg!(windows) {
            assert!(!text.contains("live-credential-value"), "{text}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let (loaded, outdated) = load_session_at(&path).unwrap();
        assert_eq!(loaded.token, "live-credential-value");
        assert_eq!(loaded.checked_at, 42);
        assert!(!outdated);
        // No temporary file is left beside it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An older build's file (the credential in clear) still signs in, and is
    /// flagged so it is rewritten protected.
    #[test]
    fn an_older_plaintext_session_file_loads_and_is_marked_for_rewrite() {
        let dir = session_dir("legacy");
        let path = dir.join(SESSION_FILE);
        std::fs::write(
            &path,
            r#"{"token":"old-credential","checked_at":7,"user":{"id":"u1","email":null,"username":"jane","avatar_url":null}}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let (loaded, outdated) = load_session_at(&path).unwrap();
        assert_eq!(loaded.token, "old-credential");
        assert!(outdated);
        store_session_at(&path, &loaded).unwrap();
        let (again, outdated) = load_session_at(&path).unwrap();
        assert_eq!(again.token, "old-credential");
        assert!(!outdated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plaintext account service would put a live session credential on the
    /// wire in clear. Release builds have no such path at all.
    #[test]
    fn plaintext_remote_endpoints_are_never_acceptable() {
        assert!(!is_acceptable_endpoint("http://auth.example.com"));
        assert!(is_acceptable_endpoint("https://futureboard.studio"));
    }
}
