//! The `fbrd://` URL scheme: how the outside world opens something in Studio.
//!
//! ```txt
//! browser / OS ──▶ fbrd://auth/callback?fbstate=…&fbsession=…  ──▶ finish a sign-in
//!                  fbrd://jam/j/J8KM4V#secret                  ──▶ join a jam
//! ```
//!
//! Two links today, both of which exist because a browser cannot otherwise
//! reach a running desktop app:
//!
//! * **Sign-in.** The account service redirects the browser to this scheme
//!   with the session and the nonce Studio minted. The loopback listener in
//!   [`crate::auth`] does the same job when the scheme is not registered — a
//!   `cargo run` on a fresh machine, a portable install — so this is the path
//!   an *installed* Studio takes, not the only one.
//! * **Jam invites.** A `fbrd://jam/j/<code>#<secret>` link on a web page opens
//!   the Audio Jam window and joins. The secret rides in the fragment for the
//!   same reason it does on the web link: it never reaches a log.
//!
//! # One instance
//!
//! The OS launches a *new* process for a scheme URL. On macOS the running app
//! receives it through Apple events, which GPUI already surfaces as
//! [`gpui::App::on_open_urls`]. On Windows and Linux there is no such delivery,
//! so the fresh process hands the URL to the one already running over a
//! loopback socket and exits — see [`forward_to_running_instance`]. The port
//! lives in a file in the per-user app-data directory, which is what makes it
//! discoverable and what keeps another user's session from finding it.
//!
//! Nothing here touches audio, and nothing here trusts the URL: a sign-in
//! callback is checked against the nonce Studio issued, and a jam link is only
//! ever handed to the same parser a pasted link goes through.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The scheme, without the `://`.
pub const SCHEME: &str = "fbrd";

/// Where the running instance publishes its relay port.
const RELAY_PORT_FILE: &str = "fbrd-relay.port";

/// How long a second instance waits for the first to acknowledge a URL before
/// deciding nobody is running and opening on its own.
const RELAY_TIMEOUT: Duration = Duration::from_millis(1500);

/// Longest line the relay reads. A link is far shorter; anything longer is not
/// one and is cut off rather than buffered.
const MAX_RELAY_LINE: usize = 8 * 1024;

/// Relay connections served at once. Each one ends within [`RELAY_TIMEOUT`],
/// so a full house frees up quickly; past it, a connection is closed unread.
const MAX_RELAY_CONNECTIONS: usize = 16;

/// What a parsed link asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    /// The account service handing back a session (or a refusal).
    AuthCallback {
        /// The nonce Studio minted when it started the sign-in.
        state: String,
        /// The opaque session credential. Empty on a refusal.
        session: String,
        /// The service's reason, when it refused.
        error: Option<String>,
    },
    /// A jam link, exactly as [`crate::jam::parse_jam_link`] accepts it.
    Jam { link: String },
}

/// Whether a string is a link in this scheme.
pub fn is_scheme_url(raw: &str) -> bool {
    let raw = raw.trim();
    raw.len() > SCHEME.len() + 3
        && raw[..SCHEME.len()].eq_ignore_ascii_case(SCHEME)
        && raw[SCHEME.len()..].starts_with("://")
}

/// Take a link apart. `None` is a link this build does not understand, which is
/// ignored rather than guessed at.
pub fn parse(raw: &str) -> Option<DeepLink> {
    let raw = raw.trim();
    if !is_scheme_url(raw) {
        return None;
    }
    let rest = &raw[SCHEME.len() + 3..];
    let (without_fragment, _) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = without_fragment
        .split_once('?')
        .unwrap_or((without_fragment, ""));
    let path = path.trim_matches('/');

    match path {
        "auth/callback" => {
            let params = crate::auth::parse_query(query);
            let get = |key: &str| {
                params
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
            };
            Some(DeepLink::AuthCallback {
                state: get("fbstate").unwrap_or_default(),
                session: get("fbsession").unwrap_or_default(),
                error: get("error").filter(|error| !error.is_empty()),
            })
        }
        // `fbrd://jam/j/CODE#secret`, or `fbrd://jam/CODE#secret`. Both are
        // handed on whole: the jam link parser owns the shape, and the fragment
        // it needs is still attached.
        _ if path == "jam" || path.starts_with("jam/") => Some(DeepLink::Jam {
            link: raw.to_string(),
        }),
        _ => None,
    }
}

/// The first scheme URL on a command line, if any.
pub fn url_from_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Option<String> {
    args.into_iter()
        .filter_map(|arg| arg.into_string().ok())
        .find(|arg| is_scheme_url(arg))
}

// ── The pending queue ────────────────────────────────────────────────────────

fn pending() -> &'static Mutex<VecDeque<String>> {
    static PENDING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Queue a URL for the UI thread to act on. Called by the relay listener, by
/// the platform's own open-URL delivery, and at startup with the URL this
/// process was launched with.
pub fn enqueue(url: impl Into<String>) {
    let url = url.into();
    if !is_scheme_url(&url) {
        return;
    }
    if let Ok(mut queue) = pending().lock() {
        queue.push_back(url);
    }
}

/// Everything queued since the last call. Never blocks, so the shell can poll
/// it from a frame.
pub fn drain() -> Vec<String> {
    pending()
        .lock()
        .map(|mut queue| queue.drain(..).collect())
        .unwrap_or_default()
}

// ── Single instance relay ────────────────────────────────────────────────────

fn relay_port_path() -> PathBuf {
    crate::paths::FutureboardPaths::resolve()
        .app_data
        .join(RELAY_PORT_FILE)
}

/// Hand a URL to an already-running Studio. `true` means it took it and this
/// process should exit; `false` means nothing is listening and this process is
/// the one that should open.
pub fn forward_to_running_instance(url: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(relay_port_path()) else {
        return false;
    };
    let Ok(port) = raw.trim().parse::<u16>() else {
        return false;
    };
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, RELAY_TIMEOUT) else {
        // A stale port file from a crashed instance. Not an error: this
        // process is about to become the instance.
        return false;
    };
    let _ = stream.set_read_timeout(Some(RELAY_TIMEOUT));
    let _ = stream.set_write_timeout(Some(RELAY_TIMEOUT));
    if writeln!(stream, "{url}").is_err() || stream.flush().is_err() {
        return false;
    }
    let mut reply = String::new();
    let mut reader = BufReader::new(&stream);
    reader.read_line(&mut reply).is_ok() && reply.trim() == "ok"
}

/// Start listening for URLs from later launches. Idempotent; call once from
/// the instance that owns the windows.
///
/// The port is chosen by the OS and published to the port file, which is
/// overwritten so a stale one from a crash can never point a new launch at
/// something else's socket.
pub fn start_relay_listener() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let listener = match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))) {
        Ok(listener) => listener,
        Err(error) => {
            crate::boot::log(&format!(
                "fbrd relay: could not bind a loopback port: {error}"
            ));
            return;
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            crate::boot::log(&format!("fbrd relay: could not read the port: {error}"));
            return;
        }
    };
    let path = relay_port_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, port.to_string()) {
        crate::boot::log(&format!("fbrd relay: could not publish the port: {error}"));
        return;
    }
    crate::boot::log(&format!("fbrd relay listening on 127.0.0.1:{port}"));

    let spawned = std::thread::Builder::new()
        .name("fbrd-relay".to_string())
        .spawn(move || serve_relay(listener, Arc::new(|url: String| enqueue(url))));
    if let Err(error) = spawned {
        crate::boot::log(&format!("fbrd relay: could not start: {error}"));
    }
}

/// The relay's accept loop. It only accepts: each connection is read and
/// answered on its own short-lived thread, so one that stalls or trickles
/// bytes cannot hold up the link hand-offs behind it.
fn serve_relay(listener: TcpListener, deliver: Arc<dyn Fn(String) + Send + Sync>) {
    let serving = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        if serving.load(Ordering::Acquire) >= MAX_RELAY_CONNECTIONS {
            continue; // dropped unread
        }
        serving.fetch_add(1, Ordering::AcqRel);
        let deliver = deliver.clone();
        let done = serving.clone();
        let spawned = std::thread::Builder::new()
            .name("fbrd-relay-connection".to_string())
            .spawn(move || {
                serve_relay_connection(stream, deliver.as_ref());
                done.fetch_sub(1, Ordering::AcqRel);
            });
        if spawned.is_err() {
            serving.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Read one link from a relay connection and acknowledge it. The whole
/// exchange has [`RELAY_TIMEOUT`], not each read, and the line is capped at
/// [`MAX_RELAY_LINE`].
fn serve_relay_connection(mut stream: TcpStream, deliver: &(dyn Fn(String) + Send + Sync)) {
    // Accepted from a blocking listener, but be explicit: a timeout needs a
    // blocking socket.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_write_timeout(Some(RELAY_TIMEOUT));
    let until = Instant::now() + RELAY_TIMEOUT;
    match read_relay_line(&mut stream, until) {
        Some(line) if is_scheme_url(&line) => {
            deliver(line.trim().to_string());
            let _ = writeln!(stream, "ok");
        }
        _ => {
            let _ = writeln!(stream, "refused");
        }
    }
    let _ = stream.flush();
}

/// One newline-terminated line (or everything before the peer closed), read
/// until `until`. `None` when it stalls past the deadline, grows past
/// [`MAX_RELAY_LINE`], or is not UTF-8.
fn read_relay_line(stream: &mut TcpStream, until: Instant) -> Option<String> {
    let mut line = Vec::with_capacity(256);
    let mut chunk = [0u8; 1024];
    loop {
        let left = until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())?;
        stream.set_read_timeout(Some(left)).ok()?;
        let read = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        let received = &chunk[..read];
        let (part, complete) = match received.iter().position(|&byte| byte == b'\n') {
            Some(end) => (&received[..end], true),
            None => (received, false),
        };
        if line.len() + part.len() > MAX_RELAY_LINE {
            return None;
        }
        line.extend_from_slice(part);
        if complete {
            break;
        }
    }
    String::from_utf8(line).ok()
}

// ── Registration ─────────────────────────────────────────────────────────────

/// Whether the OS will hand `fbrd://` links to this app.
///
/// This is what decides whether a sign-in asks the account service for the
/// scheme or for a loopback port: asking for a scheme nobody handles leaves
/// the browser showing "no application" and the app waiting forever.
pub fn scheme_registered() -> bool {
    REGISTERED.load(Ordering::Acquire)
}

static REGISTERED: AtomicBool = AtomicBool::new(false);

/// Make sure the scheme reaches this executable, where the platform lets a
/// running app do that. Call once at boot, off the hot path.
///
/// * **Windows** — a per-user `HKCU\Software\Classes\fbrd` key pointing at
///   this executable. The installer writes the same keys for an installed
///   copy; doing it here as well is what makes a development build and a
///   portable copy work, and it re-points a stale key after the app moved.
/// * **macOS** — the bundle's `Info.plist` declares the scheme, and Launch
///   Services registers it when the app first runs from a bundle; the shell
///   additionally asks GPUI to set this app as the handler.
/// * **Linux** — the `.desktop` entry declares `x-scheme-handler/fbrd`; a
///   process cannot usefully register itself, so this only reports.
pub fn ensure_registered() {
    let registered = platform_register();
    REGISTERED.store(registered, Ordering::Release);
    crate::boot::log(&format!(
        "fbrd scheme {}",
        if registered {
            "registered"
        } else {
            "not registered; sign-in will use the loopback listener"
        }
    ));
}

/// Record the result of a registration the platform did on the shell's behalf
/// (macOS, through GPUI).
pub fn mark_registered(registered: bool) {
    REGISTERED.store(registered, Ordering::Release);
}

#[cfg(target_os = "windows")]
fn platform_register() -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_string();
    let command = format!("\"{exe}\" \"%1\"");
    let root = format!("HKCU\\Software\\Classes\\{SCHEME}");

    // `reg.exe` rather than a registry crate: it is on every Windows install,
    // and the four writes here are the whole of what this needs.
    let writes: [(String, Vec<String>); 4] = [
        (
            root.clone(),
            vec![
                "/ve".into(),
                "/t".into(),
                "REG_SZ".into(),
                "/d".into(),
                "URL:Futureboard Studio".into(),
            ],
        ),
        (
            root.clone(),
            vec![
                "/v".into(),
                "URL Protocol".into(),
                "/t".into(),
                "REG_SZ".into(),
                "/d".into(),
                String::new(),
            ],
        ),
        (
            format!("{root}\\DefaultIcon"),
            vec![
                "/ve".into(),
                "/t".into(),
                "REG_SZ".into(),
                "/d".into(),
                format!("{exe},0"),
            ],
        ),
        (
            format!("{root}\\shell\\open\\command"),
            vec![
                "/ve".into(),
                "/t".into(),
                "REG_SZ".into(),
                "/d".into(),
                command,
            ],
        ),
    ];
    for (key, args) in writes {
        let status = std::process::Command::new("reg.exe")
            .arg("add")
            .arg(&key)
            .args(&args)
            .arg("/f")
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        if !status.map(|status| status.success()).unwrap_or(false) {
            return false;
        }
    }
    true
}

#[cfg(target_os = "macos")]
fn platform_register() -> bool {
    // The scheme is declared in Info.plist, so a copy that runs from a bundle
    // is registered by Launch Services on first launch. A bare binary from
    // `cargo run` has no bundle and no plist, and no way to get one at runtime.
    std::env::current_exe()
        .map(|exe| exe.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn platform_register() -> bool {
    // Ask the desktop which .desktop entry owns the scheme. Any answer means
    // some Futureboard launcher will take the link; the installer's entry
    // declares it and `Exec=… %U` passes it on.
    std::process::Command::new("xdg-settings")
        .args(["get", "default-url-scheme-handler", SCHEME])
        .output()
        .map(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .to_ascii_lowercase()
                    .contains("futureboard")
        })
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn platform_register() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_auth_callback_is_taken_apart_into_state_session_and_error() {
        let link = parse("fbrd://auth/callback?fbstate=abc-DEF_123&fbsession=11111111-2222")
            .expect("parses");
        assert_eq!(
            link,
            DeepLink::AuthCallback {
                state: "abc-DEF_123".to_string(),
                session: "11111111-2222".to_string(),
                error: None,
            }
        );
        let refused =
            parse("fbrd://auth/callback?fbstate=abc&error=access_denied").expect("parses");
        assert_eq!(
            refused,
            DeepLink::AuthCallback {
                state: "abc".to_string(),
                session: String::new(),
                error: Some("access_denied".to_string()),
            }
        );
    }

    #[test]
    fn a_jam_link_is_handed_on_whole_with_its_secret() {
        let link = parse("fbrd://jam/j/J8KM4V#u3Qb1n7").expect("parses");
        assert_eq!(
            link,
            DeepLink::Jam {
                link: "fbrd://jam/j/J8KM4V#u3Qb1n7".to_string()
            }
        );
        // And the jam link parser agrees about what it names.
        let parsed = crate::jam::parse_jam_link("fbrd://jam/j/J8KM4V#u3Qb1n7").expect("a jam");
        assert_eq!(parsed.code, "J8KM4V");
        assert_eq!(parsed.secret.as_deref(), Some("u3Qb1n7"));
    }

    #[test]
    fn anything_else_is_refused_rather_than_guessed_at() {
        assert!(parse("https://futureboard.studio/j/J8KM4V").is_none());
        assert!(parse("fbrd://open/anything").is_none());
        assert!(parse("fbrd://").is_none());
        assert!(parse("FBRD://auth/callback?fbstate=x&fbsession=y").is_some());
        assert!(!is_scheme_url("fbrd:/auth"));
    }

    #[test]
    fn the_scheme_url_is_picked_out_of_a_command_line() {
        let args = ["--flag", "C:\\song.fbproj", "fbrd://jam/j/AAAAAA"]
            .into_iter()
            .map(std::ffi::OsString::from);
        assert_eq!(url_from_args(args).as_deref(), Some("fbrd://jam/j/AAAAAA"));
        assert!(url_from_args([std::ffi::OsString::from("song.fbproj")]).is_none());
    }

    #[test]
    fn the_queue_only_ever_holds_scheme_urls() {
        enqueue("https://example.com");
        enqueue("fbrd://jam/j/QUEUED");
        let drained = drain();
        assert!(drained.contains(&"fbrd://jam/j/QUEUED".to_string()));
        assert!(!drained.iter().any(|url| url.starts_with("https")));
        assert!(drain().is_empty());
    }

    /// A relay on a private port that hands accepted links to a channel, not
    /// the process-wide queue other tests drain.
    fn test_relay() -> (SocketAddr, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        let sender = Mutex::new(sender);
        std::thread::spawn(move || {
            serve_relay(
                listener,
                Arc::new(move |url| {
                    let _ = sender.lock().unwrap().send(url);
                }),
            )
        });
        (address, receiver)
    }

    fn reply_to(stream: &TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reply = String::new();
        let _ = BufReader::new(stream).read_line(&mut reply);
        reply.trim().to_string()
    }

    /// A connection that trickles bytes without ever ending its line must not
    /// hold up the hand-off behind it, and is itself cut off at the deadline.
    #[test]
    fn a_slow_relay_connection_does_not_block_the_next_link() {
        let (address, delivered) = test_relay();

        let mut slow = TcpStream::connect(address).unwrap();
        let trickle = std::thread::spawn(move || {
            for _ in 0..30 {
                if slow.write_all(b"f").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            slow
        });
        std::thread::sleep(Duration::from_millis(150));

        let started = Instant::now();
        let mut link = TcpStream::connect(address).unwrap();
        link.write_all(b"fbrd://jam/j/RELAY1\n").unwrap();
        assert_eq!(reply_to(&link), "ok");
        assert!(
            started.elapsed() < RELAY_TIMEOUT,
            "the link waited behind the slow connection"
        );
        assert_eq!(
            delivered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "fbrd://jam/j/RELAY1"
        );

        let slow = trickle.join().unwrap();
        assert_ne!(reply_to(&slow), "ok");
        assert!(delivered.try_recv().is_err());
    }

    /// An endless line is refused once it passes the cap, not buffered.
    #[test]
    fn an_oversized_relay_line_is_refused() {
        let (address, delivered) = test_relay();
        let mut stream = TcpStream::connect(address).unwrap();
        let mut oversized = b"fbrd://jam/j/".to_vec();
        oversized.resize(MAX_RELAY_LINE + 64, b'A');
        let _ = stream.write_all(&oversized);
        assert_ne!(reply_to(&stream), "ok");
        assert!(delivered.recv_timeout(Duration::from_millis(300)).is_err());

        // And the relay still serves the next one.
        let mut link = TcpStream::connect(address).unwrap();
        link.write_all(b"fbrd://jam/j/RELAY2\n").unwrap();
        assert_eq!(reply_to(&link), "ok");
        assert_eq!(
            delivered.recv_timeout(Duration::from_secs(5)).unwrap(),
            "fbrd://jam/j/RELAY2"
        );
    }

    #[test]
    fn a_missing_port_file_means_nobody_is_running() {
        // The path is per-user app data; a test machine may or may not have a
        // Studio running, so only the negative is asserted through a URL the
        // relay would refuse anyway.
        assert!(!forward_to_running_instance("https://not-a-scheme-url"));
    }
}
