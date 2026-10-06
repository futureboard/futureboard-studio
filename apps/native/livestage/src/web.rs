//! The web UI's transport: the React app in `webui/` (built by Vite, embedded
//! at compile time) served over plain HTTP, and a WebSocket at `/ws` that
//! carries the same JSON commands as stdin.
//!
//! One thread accepts and one runs each connection. None of them touch the
//! engine: a WebSocket's frames go to the server's command loop as
//! [`Inbound`] messages, and what the loop has to say comes back through a
//! bounded outbox. A client that stops reading fills its outbox and is
//! dropped rather than holding up the loop; the page reconnects and starts
//! over from a fresh snapshot.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, SyncSender, TryRecvError};
use std::time::Duration;

use builtin_ui_embed::EmbeddedUiAssetTable;
use tungstenite::Message;

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
    /// A text frame from a web client.
    Web(ClientId, String),
    /// A web client connected; what is sent into the outbox reaches it.
    Joined(ClientId, SyncSender<String>),
    Left(ClientId),
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

/// Whether the page was embedded into this build.
pub fn has_assets() -> bool {
    ASSETS.index().is_some()
}

/// Listen on `addr` and serve until the process ends. Returns the address
/// actually bound (port 0 picks one).
pub fn serve(addr: SocketAddr, inbound: Sender<Inbound>) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let open = Arc::new(AtomicUsize::new(0));
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
                let id = next_id;
                next_id += 1;
                let inbound = inbound.clone();
                open.fetch_add(1, Ordering::Relaxed);
                let spawned = std::thread::Builder::new()
                    .name("livestage-web-conn".to_string())
                    .spawn({
                        let open = open.clone();
                        move || {
                            connection(stream, id, &inbound);
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

fn connection(mut stream: TcpStream, id: ClientId, inbound: &Sender<Inbound>) {
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
    head.push_str("X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n");
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
    let Ok(mut ws) = tungstenite::accept(stream) else {
        return;
    };
    // Short reads from here on: between frames the thread drains its outbox.
    let _ = ws.get_ref().stream.set_read_timeout(Some(POLL));
    let (outbox, pending) = std::sync::mpsc::sync_channel::<String>(OUTBOX);
    if inbound.send(Inbound::Joined(id, outbox)).is_err() {
        return;
    }
    'connection: loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                if inbound.send(Inbound::Web(id, text.to_string())).is_err() {
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
}
