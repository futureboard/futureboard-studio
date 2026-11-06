//! The web UI's transport: the React app in `webui/` (built by Vite, embedded
//! at compile time) served over plain HTTP, and a WebSocket at `/ws` that
//! carries the same JSON commands as stdin.
//!
//! One thread accepts and one runs each connection. None of them touch the
//! engine: a WebSocket's frames go to the server's command loop as
//! [`Inbound`] messages, and what the loop has to say comes back through a
//! bounded outbox. A client that stops reading fills its outbox and is
//! dropped rather than holding up the loop; the page reconnects and starts
//! over from a fresh snapshot. The other way round, a client that sends
//! faster than the loop takes its frames is dropped too (see [`Queued`]).
//!
//! Every request must name this server in its `Host`: an IP address,
//! `localhost`, this machine's own name (and `NAME.local`), or a name given
//! with `--web-host`. A page on some website that points its own name at
//! this machine (DNS rebinding) sends that name, and is refused before the
//! `Origin` check could be fooled by it.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use builtin_ui_embed::EmbeddedUiAssetTable;
use tungstenite::Message;
use tungstenite::protocol::WebSocketConfig;

mod assets {
    include!(concat!(env!("OUT_DIR"), "/embedded_ui_assets.rs"));
}

static ASSETS: EmbeddedUiAssetTable = EmbeddedUiAssetTable::new(assets::EMBEDDED_UI_ASSETS);

pub type ClientId = u64;

/// What reaches the command loop from outside.
pub enum Inbound {
    /// A command line typed or piped on stdin.
    Stdin(String),
    /// stdin closed: whoever drove the server is gone.
    StdinClosed,
    /// A text frame from a web client, counted against its backlog until
    /// the [`Queued`] is dropped.
    Web(ClientId, String, Queued),
    /// A web client connected from that address; what is sent into the
    /// outbox reaches it.
    Joined(ClientId, SyncSender<String>, Option<IpAddr>),
    Left(ClientId),
    /// The storage worker's answer (see `storage`).
    Storage(crate::storage::Done),
    /// An OSC packet or a MIDI message (see `remote`).
    Remote(crate::remote::RemoteIn),
}

/// Messages a client may have queued before it counts as stalled.
pub const OUTBOX: usize = 256;
/// How long a WebSocket thread waits for a frame before it looks at its
/// outbox: the most an update waits to go out.
const POLL: Duration = Duration::from_millis(8);
/// The longest request head taken.
const MAX_HEAD: usize = 16 * 1024;
/// Connections served at once; more are turned away.
const MAX_CONNECTIONS: usize = 64;
/// Connections from one address at once (this machine's own excepted), so
/// one client cannot take every slot.
const MAX_PER_PEER: usize = 16;
/// The largest WebSocket message (and frame) taken. Commands are small
/// JSON; nothing the page sends comes near this.
const MAX_MESSAGE: usize = 1024 * 1024;
/// Bytes of one client's frames waiting for the command loop. More than
/// this and the client is sending faster than the loop can take: dropped.
const MAX_BACKLOG: usize = 4 * 1024 * 1024;

/// Whether the page was embedded into this build.
pub fn has_assets() -> bool {
    ASSETS.index().is_some()
}

/// A frame on its way to the command loop: its bytes count against the
/// sending connection's backlog until this is dropped (when the loop is
/// done with it).
pub struct Queued {
    backlog: Arc<AtomicUsize>,
    bytes: usize,
}

impl Queued {
    /// Count `bytes` more against `backlog`; `None` when that would go past
    /// [`MAX_BACKLOG`].
    fn take(backlog: &Arc<AtomicUsize>, bytes: usize) -> Option<Self> {
        let before = backlog.fetch_add(bytes, Ordering::AcqRel);
        if before.saturating_add(bytes) > MAX_BACKLOG {
            backlog.fetch_sub(bytes, Ordering::AcqRel);
            return None;
        }
        Some(Self {
            backlog: backlog.clone(),
            bytes,
        })
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.backlog.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// The names a request's `Host` may carry besides IP addresses, `localhost`
/// and this machine's own name: `--web-host`'s, lower-cased.
type Names = Arc<Vec<String>>;

/// Open connections per peer address, for [`MAX_PER_PEER`].
type PerPeer = Arc<Mutex<HashMap<IpAddr, usize>>>;

/// One open connection from `peer`, counted in [`PerPeer`] until dropped.
struct PeerSlot {
    counts: PerPeer,
    peer: Option<IpAddr>,
}

impl PeerSlot {
    /// `None`: `peer` already has [`MAX_PER_PEER`] connections open.
    fn take(counts: &PerPeer, peer: Option<IpAddr>) -> Option<Self> {
        let peer = peer.map(|p| p.to_canonical());
        // This machine (a browser here, or a proxy in front) is never
        // limited this way.
        let counted = peer.filter(|p| !p.is_loopback());
        if let Some(peer) = counted {
            let mut map = counts.lock().unwrap_or_else(|e| e.into_inner());
            let n = map.entry(peer).or_insert(0);
            if *n >= MAX_PER_PEER {
                return None;
            }
            *n += 1;
        }
        Some(Self {
            counts: counts.clone(),
            peer: counted,
        })
    }
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let Some(peer) = self.peer else {
            return;
        };
        let mut map = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&peer);
            }
        }
    }
}

/// Listen on `addr` and serve until the process ends. Returns the address
/// actually bound (port 0 picks one). `names`: what `--web-host` added to
/// the names a request may reach this server by.
pub fn serve(
    addr: SocketAddr,
    inbound: Sender<Inbound>,
    names: Vec<String>,
) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let open = Arc::new(AtomicUsize::new(0));
    let names: Names = Arc::new(names);
    let per_peer: PerPeer = Arc::default();
    std::thread::Builder::new()
        .name("livestage-web".to_string())
        .spawn(move || {
            let mut next_id: ClientId = 1;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                if open.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    let _ = respond(&mut stream, 503, "Service Unavailable", &[], b"busy");
                    continue;
                }
                let Some(slot) = PeerSlot::take(&per_peer, stream.peer_addr().ok().map(|a| a.ip()))
                else {
                    let _ = respond(&mut stream, 503, "Service Unavailable", &[], b"busy");
                    continue;
                };
                let id = next_id;
                next_id += 1;
                let inbound = inbound.clone();
                let names = names.clone();
                open.fetch_add(1, Ordering::Relaxed);
                let spawned = std::thread::Builder::new()
                    .name("livestage-web-conn".to_string())
                    .spawn({
                        let open = open.clone();
                        move || {
                            connection(stream, id, &inbound, &names);
                            drop(slot);
                            open.fetch_sub(1, Ordering::Relaxed);
                        }
                    });
                if spawned.is_err() {
                    open.fetch_sub(1, Ordering::Relaxed);
                }
            }
        })?;
    Ok(bound)
}

/// The host part of a `Host` header (or of `--web-host`): no port, no
/// brackets around an IPv6 address, no trailing dot, lower-cased. `None`:
/// not a host.
pub fn host_name(value: &str) -> Option<String> {
    let value = value.trim();
    let host = if let Some(rest) = value.strip_prefix('[') {
        // [v6] or [v6]:port
        let (inside, after) = rest.split_once(']')?;
        if !(after.is_empty() || after.starts_with(':')) {
            return None;
        }
        inside
    } else {
        match value.rsplit_once(':') {
            // A bare IPv6 address has more than one colon: no port to take.
            Some((host, port)) if !host.contains(':') => {
                if !port.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                host
            }
            _ => value,
        }
    };
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let valid = host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'%'));
    valid.then_some(host)
}

/// Whether a request's `Host` names this server (see the module notes).
/// No `Host` at all is not a browser (every browser sends one): let in.
fn host_allowed(host: Option<&str>, names: &[String], machine: Option<&str>) -> bool {
    let Some(host) = host else {
        return true;
    };
    let Some(host) = host_name(host) else {
        return false;
    };
    if host.parse::<IpAddr>().is_ok() || host == "localhost" {
        return true;
    }
    if let Some(machine) = machine.map(str::to_ascii_lowercase) {
        if host == machine || host.strip_suffix(".local") == Some(machine.as_str()) {
            return true;
        }
    }
    names.iter().any(|name| *name == host)
}

/// This machine's name, as the network knows it (the appliance's setup
/// sets it; `NAME.local` is what its mDNS answers to).
fn machine_name() -> Option<String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is as long as said; gethostname writes at most
        // that much and the name ends at the first NUL (or the buffer's end).
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let name = std::str::from_utf8(&buf[..end]).ok()?.trim();
        // A fully qualified name: its first label is the machine's.
        let name = name.split('.').next().unwrap_or(name);
        (!name.is_empty()).then(|| name.to_string())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        std::env::var("COMPUTERNAME")
            .ok()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
    }
}

const WRONG_HOST_PAGE: &[u8] =
    b"This LiveStage server does not answer to that name. Open it by its \
IP address or NAME.local, or start the server with --web-host NAME for this name.";

fn connection(mut stream: TcpStream, id: ClientId, inbound: &Sender<Inbound>, names: &[String]) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_nodelay(true);
    let Some((bytes, head_len)) = read_head(&mut stream) else {
        return;
    };
    let Some(request) = Request::parse(&bytes[..head_len]) else {
        let _ = respond(&mut stream, 400, "Bad Request", &[], b"bad request");
        return;
    };
    // Asked for by name each time: the appliance's setup may rename it.
    if !host_allowed(request.header("host"), names, machine_name().as_deref()) {
        let _ = respond(
            &mut stream,
            421,
            "Misdirected Request",
            &[("Content-Type", "text/plain; charset=utf-8")],
            WRONG_HOST_PAGE,
        );
        return;
    }
    if request.path == "/ws" && request.wants_websocket() {
        // A page on another site must not drive the mixer through the
        // browser of someone who has it open.
        if !request.same_origin() {
            let _ = respond(&mut stream, 403, "Forbidden", &[], b"cross-origin");
            return;
        }
        // The handshake re-reads the head, so hand it back what we took.
        websocket(
            Replay {
                head: bytes,
                at: 0,
                stream,
            },
            id,
            inbound,
        );
    } else {
        let _ = serve_asset(&mut stream, &request);
    }
}

/// Read until the end of the request head. Returns everything read and
/// where the head ends.
fn read_head(stream: &mut TcpStream) -> Option<(Vec<u8>, usize)> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        let searched_from = bytes.len().saturating_sub(3);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(at) = bytes[searched_from..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
        {
            return Some((bytes, searched_from + at + 4));
        }
        if bytes.len() > MAX_HEAD {
            return None;
        }
    }
}

struct Request {
    method: String,
    path: String,
    /// Lower-cased names.
    headers: HashMap<String, String>,
}

impl Request {
    fn parse(head: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(head).ok()?;
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split_whitespace();
        let method = first.next()?.to_string();
        let target = first.next()?;
        let path = target.split(['?', '#']).next().unwrap_or("/").to_string();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        Some(Self {
            method,
            path,
            headers,
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn wants_websocket(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    }

    /// No `Origin` (not a browser), or one naming this very server.
    fn same_origin(&self) -> bool {
        let Some(origin) = self.header("origin") else {
            return true;
        };
        let Some(host) = self.header("host") else {
            return false;
        };
        let origin_host = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"))
            .unwrap_or(origin);
        origin_host.eq_ignore_ascii_case(host)
    }
}

fn serve_asset(stream: &mut TcpStream, request: &Request) -> io::Result<()> {
    let head_only = match request.method.as_str() {
        "GET" => false,
        "HEAD" => true,
        _ => return respond(stream, 405, "Method Not Allowed", &[], b""),
    };
    if !has_assets() {
        let page = NOT_BUILT_PAGE.as_bytes();
        return respond(
            stream,
            503,
            "Service Unavailable",
            &[("Content-Type", "text/html; charset=utf-8")],
            page,
        );
    }
    let Some(asset) = ASSETS.resolve(&request.path) else {
        return respond(stream, 404, "Not Found", &[], b"not found");
    };
    let etag = asset.etag.map(|tag| format!("\"{tag}\""));
    if let (Some(etag), Some(wanted)) = (&etag, request.header("if-none-match")) {
        if wanted == etag {
            return respond(stream, 304, "Not Modified", &[("ETag", etag)], b"");
        }
    }
    // Vite names bundles by content: those never change under their name.
    let cache = if asset.path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut headers = vec![("Content-Type", asset.mime_type), ("Cache-Control", cache)];
    if let Some(etag) = &etag {
        headers.push(("ETag", etag));
    }
    if head_only {
        write_head(stream, 200, "OK", &headers, asset.bytes.len())?;
        return stream.flush();
    }
    respond(stream, 200, "OK", &headers, asset.bytes)
}

const NOT_BUILT_PAGE: &str = "<!doctype html><meta charset=utf-8><title>LiveStage</title>\
<body style=\"font:15px system-ui;background:#16181F;color:#EBEDF2;padding:32px\">\
<h1>LiveStage web UI is not in this build</h1>\
<p>Build the page, then rebuild the server:</p>\
<pre style=\"background:#0E1015;padding:12px\">bun install\n\
bun run --cwd apps/native/livestage/webui build\n\
cargo build -p livestage --bin livestage-server</pre>\
<p>The WebSocket at <code>/ws</code> works without it.</p></body>";

fn write_head(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    headers: &[(&str, &str)],
    length: usize,
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: {length}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    // Never inside another site's frame, where a page could trick a click
    // onto the mixer.
    head.push_str(
        "X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\n\
         Content-Security-Policy: frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
    );
    stream.write_all(head.as_bytes())
}

fn respond(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    write_head(stream, code, reason, headers, body.len())?;
    stream.write_all(body)?;
    stream.flush()
}

/// The connection with the bytes already read put back in front.
struct Replay {
    head: Vec<u8>,
    at: usize,
    stream: TcpStream,
}

impl Read for Replay {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at < self.head.len() {
            let n = buf.len().min(self.head.len() - self.at);
            buf[..n].copy_from_slice(&self.head[self.at..self.at + n]);
            self.at += n;
            return Ok(n);
        }
        self.stream.read(buf)
    }
}

impl Write for Replay {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn is_timeout(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(e)
        if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

fn websocket(stream: Replay, id: ClientId, inbound: &Sender<Inbound>) {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let Ok(mut ws) = tungstenite::accept_with_config(stream, Some(config)) else {
        return;
    };
    let backlog = Arc::new(AtomicUsize::new(0));
    // Short reads from here on: between frames the thread drains its outbox.
    let _ = ws.get_ref().stream.set_read_timeout(Some(POLL));
    let (outbox, pending) = std::sync::mpsc::sync_channel::<String>(OUTBOX);
    // Failed log-ins are counted per address.
    let peer = ws.get_ref().stream.peer_addr().ok().map(|a| a.ip());
    if inbound.send(Inbound::Joined(id, outbox, peer)).is_err() {
        return;
    }
    'connection: loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                // Sending faster than the loop takes it in: drop the client
                // rather than let its frames pile up in memory.
                let Some(queued) = Queued::take(&backlog, text.len()) else {
                    break;
                };
                if inbound
                    .send(Inbound::Web(id, text.to_string(), queued))
                    .is_err()
                {
                    break;
                }
            }
            Ok(Message::Close(_)) => break,
            // Pings are answered by tungstenite on the next flush.
            Ok(_) => {}
            Err(error) if is_timeout(&error) => {}
            Err(_) => break,
        }
        loop {
            match pending.try_recv() {
                Ok(text) => {
                    if ws.write(Message::text(text)).is_err() {
                        break 'connection;
                    }
                }
                Err(TryRecvError::Empty) => break,
                // The command loop dropped this client (it fell behind).
                Err(TryRecvError::Disconnected) => break 'connection,
            }
        }
        match ws.flush() {
            Ok(()) => {}
            Err(error) if is_timeout(&error) => {}
            Err(_) => break,
        }
    }
    let _ = ws.close(None);
    let _ = ws.flush();
    let _ = inbound.send(Inbound::Left(id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(head: &str) -> Request {
        Request::parse(head.as_bytes()).unwrap()
    }

    #[test]
    fn a_request_head_parses() {
        let r = request(
            "GET /assets/app.js?v=2 HTTP/1.1\r\nHost: 10.0.0.5:8730\r\nUpgrade: WebSocket\r\n",
        );
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/assets/app.js");
        assert_eq!(r.header("host"), Some("10.0.0.5:8730"));
        assert!(r.wants_websocket());
    }

    #[test]
    fn only_this_server_may_open_the_socket_from_a_browser() {
        let ok =
            request("GET /ws HTTP/1.1\r\nHost: 10.0.0.5:8730\r\nOrigin: http://10.0.0.5:8730\r\n");
        assert!(ok.same_origin());
        let tool = request("GET /ws HTTP/1.1\r\nHost: 10.0.0.5:8730\r\n");
        assert!(tool.same_origin());
        let other =
            request("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:8730\r\nOrigin: https://evil.example\r\n");
        assert!(!other.same_origin());
    }

    #[test]
    fn host_names_lose_their_port_brackets_and_case() {
        assert_eq!(host_name("10.0.0.5:8730").as_deref(), Some("10.0.0.5"));
        assert_eq!(
            host_name("LiveStage.local.:8730").as_deref(),
            Some("livestage.local")
        );
        assert_eq!(host_name("[::1]:8730").as_deref(), Some("::1"));
        assert_eq!(host_name("::1").as_deref(), Some("::1"));
        assert_eq!(host_name("localhost").as_deref(), Some("localhost"));
        assert_eq!(host_name("evil.example:80x"), None);
        assert_eq!(host_name("a b"), None);
        assert_eq!(host_name("[::1]x"), None);
    }

    #[test]
    fn only_names_of_this_server_are_answered() {
        let names = vec!["mixer.example.org".to_string()];
        let machine = Some("LiveStage");
        let allowed = |host: Option<&str>| host_allowed(host, &names, machine);
        // Addresses, this machine's name, and what --web-host added.
        for host in [
            "10.0.0.5:8730",
            "127.0.0.1:8730",
            "[fe80::1]:8730",
            "localhost:8730",
            "livestage:8730",
            "LIVESTAGE.local:8730",
            "mixer.example.org",
        ] {
            assert!(allowed(Some(host)), "{host}");
        }
        // Not a browser.
        assert!(allowed(None));
        // A rebinding page sends its own name, whatever it resolves to.
        for host in [
            "evil.example:8730",
            "livestage.evil.example",
            "localhost.evil.example",
            "other.local:8730",
            "",
        ] {
            assert!(!allowed(Some(host)), "{host}");
        }
        assert!(!host_allowed(Some("livestage.local"), &[], None));
    }

    #[test]
    fn a_backlog_refuses_past_its_limit_and_frees_as_frames_are_handled() {
        let backlog = Arc::new(AtomicUsize::new(0));
        let first = Queued::take(&backlog, MAX_BACKLOG - 10).unwrap();
        assert!(Queued::take(&backlog, 11).is_none());
        assert_eq!(backlog.load(Ordering::Relaxed), MAX_BACKLOG - 10);
        let second = Queued::take(&backlog, 10).unwrap();
        drop(first);
        drop(second);
        assert_eq!(backlog.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn one_address_cannot_take_every_connection() {
        let counts: PerPeer = Arc::default();
        let peer: IpAddr = "10.0.0.9".parse().unwrap();
        let slots: Vec<_> = (0..MAX_PER_PEER)
            .map(|_| PeerSlot::take(&counts, Some(peer)).unwrap())
            .collect();
        assert!(PeerSlot::take(&counts, Some(peer)).is_none());
        assert!(PeerSlot::take(&counts, Some("10.0.0.10".parse().unwrap())).is_some());
        // This machine is never limited.
        let local: Vec<_> = (0..MAX_PER_PEER + 1)
            .map(|_| PeerSlot::take(&counts, Some("127.0.0.1".parse().unwrap())))
            .collect();
        assert!(local.iter().all(Option::is_some));
        drop(slots);
        assert!(PeerSlot::take(&counts, Some(peer)).is_some());
    }
}
