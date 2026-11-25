//! Futureboard API server: a local HTTP + WebSocket endpoint that streams the
//! master bus's loudness and visualizer data, and serves the Loudness &
//! Visualizer web page built from `apps/native/studio/visualizer-webui`.
//!
//! Off unless Settings → Experimental turns it on. It listens on the loopback
//! interface only, so nothing on the network reaches it, and refuses requests
//! whose `Host` is not a loopback name (a web page elsewhere cannot rebind a
//! name of its own onto it) and WebSocket upgrades from another origin.
//!
//! Everything here is read-only. It reads the post-fader master tap
//! (`DirectAudio::visualizer_tap`) through its own [`Analyzer`], exactly as a
//! visualizer window does, so it never takes the mixer's meter peaks and never
//! runs on the audio thread. The analysis runs only while a client is
//! connected, or for a few seconds after an HTTP snapshot was asked for.
//!
//! Model: plain threads and blocking `tungstenite`, like LiveStage's web
//! server — the studio gains no async runtime. One accept thread, one thread
//! per connection (at most [`MAX_CLIENTS`]), one analysis thread.
//!
//! # API (version 1)
//!
//! - `GET /api/v1/status` → `{ "app", "api", "version", "clients", "live", "sampleRate" }`
//! - `GET /api/v1/loudness` → the latest [`frame`](#frames) (`live: false`
//!   until the analysis has data)
//! - `POST /api/v1/loudness/reset` → restarts the integrated loudness, range
//!   and true peak
//! - `GET /ws` (WebSocket) → a frame about 30 times a second, a `history`
//!   message once a second. The client may send `{"type":"reset-loudness"}`.
//! - Anything else under `/` → the embedded web page.
//!
//! ## Frames
//!
//! `{"type":"frame","seq","live","sampleRate",
//!   "loudness":{"momentary","shortTerm","integrated","range","truePeak"},
//!   "levels":{"rms":[l,r],"peak":[l,r],"correlation","balance"},
//!   "spectrum":[256 dB],"peaks":[256 dB],"scope":[[l,r]…]}`
//!
//! Loudness values are LUFS / LU / dBTP, `null` while there is not enough
//! audio to measure. Spectrum bins run log-spaced from 20 Hz to 20 kHz,
//! −90..0 dB with a 4.5 dB/oct tilt, as the in-app spectrum draws them.
//! `history` is `{"type":"history","step":0.1,"shortTerm":[LUFS…]}`, the last
//! 30 s of short-term loudness.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use builtin_ui_embed::EmbeddedUiAssetTable;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::Message;

use crate::components::visualizer::analysis::{Analyzer, Features, LOUDNESS_HISTORY_STEP};

mod assets {
    include!(concat!(env!("OUT_DIR"), "/api_webui_assets.rs"));
}

static ASSETS: EmbeddedUiAssetTable = EmbeddedUiAssetTable::new(assets::EMBEDDED_UI_ASSETS);

/// The port used when Settings names none.
pub const DEFAULT_PORT: u16 = 7471;
/// API version in every path and in `/api/v1/status`.
pub const API_VERSION: u32 = 1;
/// Concurrent connections, page loads and WebSockets together.
pub const MAX_CLIENTS: usize = 16;

const MAX_HEAD: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// One analysis step and one WebSocket frame: about 30 a second.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// A `history` message every this many frames (about once a second).
const HISTORY_EVERY: u64 = 30;
/// How long an HTTP snapshot keeps the analysis running with no WebSocket.
const SNAPSHOT_KEEPALIVE: Duration = Duration::from_secs(5);
/// Vectorscope pairs sent per frame (of the analyzer's 2048).
const SCOPE_POINTS: usize = 256;
/// Largest message a client may send (it only ever sends small commands).
const MAX_MESSAGE: usize = 4 * 1024;

/// What the server is doing, for Settings to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiServerStatus {
    Off,
    Running(SocketAddr),
    Failed(String),
}

struct Running {
    port: u16,
    addr: SocketAddr,
    shared: Arc<Shared>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Start, stop or move the server to match Settings. Called at startup and
/// whenever settings change; a call that changes nothing does nothing.
pub fn apply_settings(enabled: bool, port: u16) {
    let port = if port == 0 { DEFAULT_PORT } else { port };
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(current) = running.as_ref() {
        if enabled && current.port == port {
            return;
        }
        current.shared.stop(current.addr);
        *running = None;
    }
    *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if !enabled {
        return;
    }
    match start(port) {
        Ok((addr, shared)) => {
            eprintln!("[api-server] listening on http://{addr}");
            *running = Some(Running { port, addr, shared });
        }
        Err(error) => {
            let message = format!("port {port}: {error}");
            eprintln!("[api-server] could not start: {message}");
            *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
        }
    }
}

/// Stop the server, if it runs (app exit).
pub fn shutdown() {
    apply_settings(false, 0);
}

pub fn status() -> ApiServerStatus {
    if let Some(running) = RUNNING.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return ApiServerStatus::Running(running.addr);
    }
    match LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        Some(error) => ApiServerStatus::Failed(error),
        None => ApiServerStatus::Off,
    }
}

/// State the accept, connection and analysis threads share.
struct Shared {
    stopping: AtomicBool,
    clients: AtomicUsize,
    /// Open WebSockets: the analysis runs while there is one.
    sockets: AtomicUsize,
    /// When an HTTP snapshot last asked for data.
    snapshot_wanted: Mutex<Option<Instant>>,
    /// Set by a client; the analysis thread restarts the loudness meter.
    reset_loudness: AtomicBool,
    /// The latest frame and history, as JSON, and their sequence number.
    latest: Mutex<Published>,
    seq: AtomicU64,
    sample_rate: AtomicU64,
    live: AtomicBool,
}

#[derive(Default)]
struct Published {
    frame: Option<Arc<str>>,
    history: Option<Arc<str>>,
}

impl Shared {
    fn stop(&self, addr: SocketAddr) {
        self.stopping.store(true, Ordering::Release);
        // Wake the blocking accept so its thread sees the flag and ends.
        let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
    }

    fn analysis_wanted(&self) -> bool {
        self.sockets.load(Ordering::Acquire) > 0
            || self
                .snapshot_wanted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|at| at.elapsed() < SNAPSHOT_KEEPALIVE)
    }
}

fn start(port: u16) -> io::Result<(SocketAddr, Arc<Shared>)> {
    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))?;
    let addr = listener.local_addr()?;
    let shared = Arc::new(Shared {
        stopping: AtomicBool::new(false),
        clients: AtomicUsize::new(0),
        sockets: AtomicUsize::new(0),
        snapshot_wanted: Mutex::new(None),
        reset_loudness: AtomicBool::new(false),
        latest: Mutex::new(Published::default()),
        seq: AtomicU64::new(0),
        sample_rate: AtomicU64::new(0),
        live: AtomicBool::new(false),
    });
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("api-server-analysis".to_string())
            .spawn(move || analysis_loop(&shared))?;
    }
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("api-server".to_string())
            .spawn(move || accept_loop(listener, &shared))?;
    }
    Ok((addr, shared))
}

fn accept_loop(listener: TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stopping.load(Ordering::Acquire) {
            break;
        }
        let Ok(mut stream) = stream else {
            continue;
        };
        if shared.clients.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
            shared.clients.fetch_sub(1, Ordering::AcqRel);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let _ = respond(&mut stream, 503, "Service Unavailable", &[], b"busy");
            continue;
        }
        let shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("api-server-conn".to_string())
            .spawn({
                let shared = shared.clone();
                move || {
                    connection(stream, &shared);
                    shared.clients.fetch_sub(1, Ordering::AcqRel);
                }
            });
        if spawned.is_err() {
            shared.clients.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

// ── Analysis ────────────────────────────────────────────────────────────────

fn analysis_loop(shared: &Shared) {
    let tap = DirectAudio::visualizer_tap();
    let mut analyzer: Option<Analyzer> = None;
    let mut cursor = 0u64;
    let mut last = Instant::now();
    let mut frames = 0u64;
    let mut scope = Vec::with_capacity(SCOPE_POINTS);
    while !shared.stopping.load(Ordering::Acquire) {
        let started = Instant::now();
        if shared.analysis_wanted() {
            let analyzer = analyzer.get_or_insert_with(|| {
                cursor = tap.listen();
                last = Instant::now();
                Analyzer::new(Features {
                    spectrum: true,
                    loudness: true,
                })
            });
            if shared.reset_loudness.swap(false, Ordering::AcqRel) {
                analyzer.reset_loudness();
            }
            let now = Instant::now();
            let dt = now.duration_since(last).as_secs_f32().clamp(0.001, 0.25);
            last = now;
            cursor = analyzer.pull(cursor, dt);
            frames += 1;
            let seq = shared.seq.fetch_add(1, Ordering::AcqRel) + 1;
            shared
                .sample_rate
                .store(u64::from(analyzer.sample_rate()), Ordering::Relaxed);
            shared.live.store(analyzer.live, Ordering::Relaxed);
            let frame: Arc<str> = frame_json(analyzer, seq, &mut scope).into();
            let history = (frames % HISTORY_EVERY == 1).then(|| Arc::from(history_json(analyzer)));
            let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            latest.frame = Some(frame);
            if let Some(history) = history {
                latest.history = Some(history);
            }
        } else if analyzer.take().is_some() {
            // Nobody is watching: stop the tap copying audio for us.
            tap.unlisten();
            shared.live.store(false, Ordering::Relaxed);
        }
        let spent = started.elapsed();
        std::thread::sleep(FRAME_INTERVAL.saturating_sub(spent));
    }
    if analyzer.is_some() {
        tap.unlisten();
    }
}

/// A JSON number, or `null` for a reading that is not there or not finite.
fn push_number(out: &mut String, value: Option<f32>) {
    match value.filter(|v| v.is_finite()) {
        Some(v) => {
            let _ = write!(out, "{}", (v * 100.0).round() / 100.0);
        }
        None => out.push_str("null"),
    }
}

fn push_array(out: &mut String, values: &[f32]) {
    out.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let value = if value.is_finite() { *value } else { -90.0 };
        let _ = write!(out, "{}", (value * 10.0).round() / 10.0);
    }
    out.push(']');
}

fn frame_json(analyzer: &Analyzer, seq: u64, scope: &mut Vec<[f32; 2]>) -> String {
    let mut out = String::with_capacity(8 * 1024);
    let _ = write!(
        out,
        "{{\"type\":\"frame\",\"seq\":{seq},\"live\":{},\"sampleRate\":{},\"loudness\":{{\"momentary\":",
        analyzer.live,
        analyzer.sample_rate()
    );
    push_number(&mut out, analyzer.momentary_lufs);
    out.push_str(",\"shortTerm\":");
    push_number(&mut out, analyzer.short_term_lufs);
    out.push_str(",\"integrated\":");
    push_number(&mut out, analyzer.integrated_lufs);
    out.push_str(",\"range\":");
    push_number(&mut out, analyzer.range_lu);
    out.push_str(",\"truePeak\":");
    push_number(&mut out, analyzer.true_peak_db);
    out.push_str("},\"levels\":{\"rms\":[");
    push_number(&mut out, Some(analyzer.rms[0]));
    out.push(',');
    push_number(&mut out, Some(analyzer.rms[1]));
    out.push_str("],\"peak\":[");
    push_number(&mut out, Some(analyzer.peak[0]));
    out.push(',');
    push_number(&mut out, Some(analyzer.peak[1]));
    out.push_str("],\"correlation\":");
    push_number(&mut out, Some(analyzer.correlation));
    out.push_str(",\"balance\":");
    push_number(&mut out, Some(analyzer.balance_db));
    out.push_str("},\"spectrum\":");
    push_array(&mut out, &analyzer.spectrum);
    out.push_str(",\"peaks\":");
    push_array(&mut out, &analyzer.peaks);
    out.push_str(",\"scope\":[");
    scope.clear();
    let pairs: Vec<[f32; 2]> = analyzer.scope_pairs().collect();
    let stride = (pairs.len() / SCOPE_POINTS).max(1);
    scope.extend(pairs.iter().step_by(stride).take(SCOPE_POINTS).copied());
    for (index, [l, r]) in scope.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let l = if l.is_finite() { *l } else { 0.0 };
        let r = if r.is_finite() { *r } else { 0.0 };
        let _ = write!(
            out,
            "[{},{}]",
            (l * 1000.0).round() / 1000.0,
            (r * 1000.0).round() / 1000.0
        );
    }
    out.push_str("]}");
    out
}

fn history_json(analyzer: &Analyzer) -> String {
    let mut out = String::with_capacity(4 * 1024);
    let _ = write!(
        out,
        "{{\"type\":\"history\",\"step\":{LOUDNESS_HISTORY_STEP},\"shortTerm\":"
    );
    push_array(&mut out, &analyzer.loudness_history);
    out.push('}');
    out
}

// ── HTTP ────────────────────────────────────────────────────────────────────

struct Request {
    method: String,
    path: String,
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
        let mut headers = HashMap::new();
        for line in lines.filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':')?;
            if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
                return None;
            }
            headers.insert(
                name.to_ascii_lowercase(),
                value.trim_matches([' ', '\t']).to_string(),
            );
        }
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
        origin
            .strip_prefix("http://")
            .is_some_and(|origin_host| origin_host.eq_ignore_ascii_case(host))
    }
}

/// Whether `host` (a `Host` header) names this loopback server: a loopback
/// address or `localhost`, any port. Anything else is a name some other site
/// pointed at 127.0.0.1 (DNS rebinding). No `Host` is not a browser: let in.
fn host_allowed(host: Option<&str>) -> bool {
    let Some(host) = host.map(str::trim) else {
        return true;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inside, _)) => inside,
            None => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    let name = name.to_ascii_lowercase();
    name == "localhost" || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn read_head(stream: &mut TcpStream) -> Option<(Vec<u8>, usize)> {
    let until = Instant::now() + HEAD_TIMEOUT;
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let left = until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())?;
        stream.set_read_timeout(Some(left)).ok()?;
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        let from = bytes.len().saturating_sub(3);
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(at) = bytes[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            return Some((bytes, from + at + 4));
        }
        if bytes.len() > MAX_HEAD {
            return None;
        }
    }
}

fn connection(mut stream: TcpStream, shared: &Arc<Shared>) {
    let Some((bytes, head_end)) = read_head(&mut stream) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let Some(request) = Request::parse(&bytes[..head_end]) else {
        let _ = respond(&mut stream, 400, "Bad Request", &[], b"bad request");
        return;
    };
    if !host_allowed(request.header("host")) {
        let _ = respond(&mut stream, 421, "Misdirected Request", &[], b"wrong host");
        return;
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/ws") if request.wants_websocket() => {
            if !request.same_origin() {
                let _ = respond(&mut stream, 403, "Forbidden", &[], b"cross-origin");
                return;
            }
            websocket(
                Replay {
                    head: bytes,
                    at: 0,
                    stream,
                },
                shared,
            );
        }
        ("GET", "/api/v1/status") => {
            let body = format!(
                "{{\"app\":\"Futureboard Studio\",\"api\":{API_VERSION},\"version\":\"{}\",\
                 \"clients\":{},\"live\":{},\"sampleRate\":{}}}",
                crate::edition::app_version(),
                shared.sockets.load(Ordering::Acquire),
                shared.live.load(Ordering::Relaxed),
                shared.sample_rate.load(Ordering::Relaxed)
            );
            let _ = respond_json(&mut stream, 200, "OK", body.as_bytes());
        }
        ("GET", "/api/v1/loudness") => {
            *shared
                .snapshot_wanted
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            let frame = shared
                .latest
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .frame
                .clone();
            let body: Arc<str> =
                frame.unwrap_or_else(|| Arc::from("{\"type\":\"frame\",\"live\":false}"));
            let _ = respond_json(&mut stream, 200, "OK", body.as_bytes());
        }
        ("POST", "/api/v1/loudness/reset") => {
            if !request.same_origin() {
                let _ = respond(&mut stream, 403, "Forbidden", &[], b"cross-origin");
                return;
            }
            shared.reset_loudness.store(true, Ordering::Release);
            let _ = respond_json(&mut stream, 200, "OK", b"{\"ok\":true}");
        }
        ("GET" | "HEAD", path) if !path.starts_with("/api/") => {
            let _ = serve_asset(&mut stream, &request);
        }
        _ => {
            let _ = respond_json(&mut stream, 404, "Not Found", b"{\"error\":\"not found\"}");
        }
    }
}

const NOT_BUILT_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Futureboard Visualizer</title>\
<body style=\"font:15px system-ui;background:#16181F;color:#EBEDF2;padding:32px\">\
<h1>The visualizer page is not in this build</h1>\
<p>Build it, then rebuild Studio:</p>\
<pre style=\"background:#0E1015;padding:12px\">bun install\n\
bun run build:visualizer-web</pre>\
<p>The API at <code>/api/v1/</code> and the WebSocket at <code>/ws</code> work without it.</p></body>";

/// The page's scripts come only from this server; Astro's island loader is
/// inline, so inline scripts are allowed — the server only ever serves its
/// own embedded files and answers loopback only. No `eval`, no framing.
const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; \
style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; \
connect-src 'self' ws://127.0.0.1:* ws://localhost:*; object-src 'none'; base-uri 'none'; \
form-action 'none'; frame-ancestors 'none'";

fn serve_asset(stream: &mut TcpStream, request: &Request) -> io::Result<()> {
    let head_only = request.method == "HEAD";
    if ASSETS.is_empty() {
        return respond(
            stream,
            503,
            "Service Unavailable",
            &[("Content-Type", "text/html; charset=utf-8")],
            NOT_BUILT_PAGE.as_bytes(),
        );
    }
    let Some(asset) = ASSETS.resolve(&request.path) else {
        return respond(stream, 404, "Not Found", &[], b"not found");
    };
    let etag = asset.etag.map(|tag| format!("\"{tag}\""));
    if let (Some(etag), Some(wanted)) = (&etag, request.header("if-none-match")) {
        if wanted == etag {
            return respond(stream, 304, "Not Modified", &[("ETag", etag.as_str())], b"");
        }
    }
    // Astro names its bundles by content.
    let cache = if asset.path.starts_with("/_astro/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut headers = vec![("Content-Type", asset.mime_type), ("Cache-Control", cache)];
    if let Some(etag) = &etag {
        headers.push(("ETag", etag.as_str()));
    }
    if head_only {
        write_head(stream, 200, "OK", &headers, asset.bytes.len())?;
        return stream.flush();
    }
    respond(stream, 200, "OK", &headers, asset.bytes)
}

fn write_head(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    headers: &[(&str, &str)],
    length: usize,
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: {length}\r\n");
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    let _ = write!(
        head,
        "Content-Security-Policy: {CSP}\r\nX-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\nConnection: close\r\n\r\n"
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

fn respond_json(stream: &mut TcpStream, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    respond(
        stream,
        code,
        reason,
        &[
            ("Content-Type", "application/json"),
            ("Cache-Control", "no-store"),
        ],
        body,
    )
}

// ── WebSocket ───────────────────────────────────────────────────────────────

/// The connection with the request head already read put back in front, for
/// the handshake to read again.
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

fn websocket(stream: Replay, shared: &Arc<Shared>) {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let Ok(mut ws) = tungstenite::accept_with_config(stream, Some(config)) else {
        return;
    };
    // Short reads: between them the thread sends the newest frame.
    let _ = ws
        .get_ref()
        .stream
        .set_read_timeout(Some(FRAME_INTERVAL / 2));
    shared.sockets.fetch_add(1, Ordering::AcqRel);
    let mut sent_frame: Option<Arc<str>> = None;
    let mut sent_history: Option<Arc<str>> = None;
    loop {
        if shared.stopping.load(Ordering::Acquire) {
            let _ = ws.close(None);
            let _ = ws.flush();
            break;
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                if text.as_str().contains("\"reset-loudness\"") {
                    shared.reset_loudness.store(true, Ordering::Release);
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(error) if is_timeout(&error) => {}
            Err(_) => break,
        }
        // Only the newest frame is ever sent: a slow client skips frames
        // instead of queueing them.
        let (frame, history) = {
            let latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            (latest.frame.clone(), latest.history.clone())
        };
        let mut wrote = false;
        if let Some(history) = history.filter(|h| !same(&sent_history, h)) {
            if ws.write(Message::text(history.to_string())).is_err() {
                break;
            }
            sent_history = Some(history);
            wrote = true;
        }
        if let Some(frame) = frame.filter(|f| !same(&sent_frame, f)) {
            if ws.write(Message::text(frame.to_string())).is_err() {
                break;
            }
            sent_frame = Some(frame);
            wrote = true;
        }
        if wrote && ws.flush().is_err() {
            break;
        }
    }
    shared.sockets.fetch_sub(1, Ordering::AcqRel);
}

fn same(sent: &Option<Arc<str>>, next: &Arc<str>) -> bool {
    sent.as_ref().is_some_and(|sent| Arc::ptr_eq(sent, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_names_reach_the_server() {
        for host in [
            "127.0.0.1:7471",
            "localhost:7471",
            "LOCALHOST",
            "[::1]:7471",
            "127.0.0.1",
        ] {
            assert!(host_allowed(Some(host)), "{host}");
        }
        for host in [
            "evil.example:7471",
            "192.168.1.5:7471",
            "localhost.evil.example",
            "[::1",
        ] {
            assert!(!host_allowed(Some(host)), "{host}");
        }
        assert!(host_allowed(None), "not a browser");
    }

    #[test]
    fn a_websocket_from_another_site_is_refused() {
        let request = |origin: &str| {
            Request::parse(
                format!("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:7471\r\nOrigin: {origin}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap()
        };
        assert!(request("http://127.0.0.1:7471").same_origin());
        assert!(!request("http://evil.example").same_origin());
        assert!(!request("https://127.0.0.1:7471").same_origin());
    }

    #[test]
    fn a_frame_is_json_with_every_reading() {
        let mut analyzer = Analyzer::new(Features {
            spectrum: true,
            loudness: true,
        });
        let tone: Vec<f32> = (0..4_800)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 1_000.0 / 48_000.0).sin() * 0.5)
            .collect();
        for _ in 0..20 {
            analyzer.feed(&tone, &tone, 0.1);
        }
        let mut scope = Vec::new();
        let frame = frame_json(&analyzer, 7, &mut scope);
        let value: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(value["type"], "frame");
        assert_eq!(value["seq"], 7);
        assert_eq!(value["spectrum"].as_array().unwrap().len(), 256);
        assert!(value["scope"].as_array().unwrap().len() <= SCOPE_POINTS);
        let momentary = value["loudness"]["momentary"]
            .as_f64()
            .expect("a loudness reading");
        assert!((-20.0..0.0).contains(&momentary), "momentary {momentary}");

        let history: serde_json::Value =
            serde_json::from_str(&history_json(&analyzer)).expect("valid JSON");
        assert_eq!(history["type"], "history");
    }

    #[test]
    fn the_server_answers_status_and_refuses_another_host() {
        let (addr, shared) = start(0).expect("bind a free loopback port");
        let get = |host: &str, path: &str| {
            let mut stream = TcpStream::connect(addr).unwrap();
            write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            answer
        };
        let status = get(&addr.to_string(), "/api/v1/status");
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        assert!(status.contains("\"api\":1"), "{status}");
        let rebound = get("evil.example", "/api/v1/status");
        assert!(rebound.starts_with("HTTP/1.1 421"), "{rebound}");
        shared.stop(addr);
    }
}
