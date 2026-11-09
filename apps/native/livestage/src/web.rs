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
//!
//! Who gets a connection slot is decided on the accept thread, before a
//! byte is read: each address's rate of new connections ([`Arrivals`]),
//! then the slot table ([`Slots`]). A connection that never logs in is
//! closed at its deadline, and may be closed earlier to make room: once past
//! its [`LOGIN_GRACE`], or at once when its address holds another connection
//! that grace protects (one per address); one let in never is. An address
//! is an IPv4 address or a whole IPv6 address (see [`peer_key`]), so devices
//! sharing a LAN's `/64` are limited one by one. On top of that, all the
//! addresses of one IPv6 `/64`, which one machine can fill with as many as
//! it likes, share a rate of new connections and at most
//! [`MAX_PROTECTED_PER_PREFIX`] connections that grace protects (see
//! [`prefix_key`]).
//!
//! Who a client is ([`Peer`]) is read from its request head. A connection
//! from this machine is taken for a browser here only when it names this
//! machine (`localhost`, a loopback address) and carries no proxy
//! forwarding header: behind a reverse proxy on this machine every browser
//! connects from loopback, and must not skip the setup code. Nor is any
//! connection taken for one once the operator says browsers reach this
//! server from elsewhere ([`reached_from_elsewhere`]): a proxy here may pass
//! the client's own `Host` on and add no forwarding header (#351), so a
//! browser here then needs the setup code too. A connection
//! from a proxy given with `--trusted-proxy` is counted (log-in failures,
//! slots and arrivals alike) under the client address that proxy forwards
//! in the one header `--forwarded-header` names, and is never taken for
//! this machine; a forwarding header from anyone else, or another header
//! from it, is not believed. Until its request head is read, a trusted
//! proxy's connection is held to no address's limits, as this machine's
//! are not: they are the client's to bear once it is known.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use builtin_ui_embed::EmbeddedUiAssetTable;
use tungstenite::protocol::WebSocketConfig;
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
    /// A text frame from a web client, counted against its backlog until
    /// the [`Queued`] is dropped.
    Web(ClientId, String, Queued),
    /// A web client connected as that [`Peer`]; what is sent into the
    /// outbox reaches it. The command loop tells the [`Admission`] when the
    /// client is let in or shut out.
    Joined(ClientId, SyncSender<String>, Peer, Admission),
    Left(ClientId),
    /// The storage worker's answer (see `storage`).
    Storage(crate::storage::Done),
    /// An OSC packet or a MIDI message (see `remote`).
    Remote(crate::remote::RemoteIn),
}

/// Who a web client is, as far as this server can tell (see the module
/// notes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    /// Its address: the connection's, or the one a trusted proxy forwarded.
    /// `None` when it could not be read.
    pub addr: Option<IpAddr>,
    /// A browser on this machine, which needs no setup code.
    pub local: bool,
}

/// Request headers a proxy adds to say who it forwards for.
const FORWARDING_HEADERS: [&str; 6] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "via",
];

/// Messages a client may have queued before it counts as stalled.
pub const OUTBOX: usize = 256;
/// How long a WebSocket thread waits for a frame before it looks at its
/// outbox: the most an update waits to go out.
const POLL: Duration = Duration::from_millis(8);
/// The longest request head taken.
const MAX_HEAD: usize = 16 * 1024;
/// Connections served at once. When all are taken, a new one makes room by
/// closing one that was never let in and that grace no longer protects (see
/// [`Slots::make_room`]); with none of those, it is turned away.
const MAX_CONNECTIONS: usize = 64;
/// Connections never let in from one address at once (this machine's own
/// excepted), so one client cannot take every slot. Those let in are not
/// counted: devices logged in behind one address are not turned away. An
/// IPv6 address counts on its own, not with the rest of its `/64`: those
/// that are evictable cannot keep anyone out (see
/// [`MAX_PROTECTED_PER_PREFIX`]).
const MAX_PER_PEER: usize = 16;
/// How long a connection may take to send its whole request head. A read
/// timeout alone is per read: a client sending a byte at a time would keep
/// its slot for ever.
const HEAD_DEADLINE: Duration = Duration::from_secs(10);
/// How long a WebSocket may stay open without being let in (logged in, or
/// past the setup code) before it is closed. The page reconnects to try
/// again; an idle one stops holding a slot.
const LOGIN_DEADLINE: Duration = Duration::from_secs(120);
/// How long a new connection (or one just logged out) is safe from being
/// closed to make room for another: time to load the page, pick a name and
/// type a PIN or the setup code, which takes seconds, with room to spare.
/// Shorter than [`LOGIN_DEADLINE`], so that a pool full of connections that
/// never log in turns over four times as fast as they would time out. A page
/// whose connection is closed after it reconnects within a second, keeping
/// what was typed, and gets a new window.
///
/// One window per address, not per connection: grace protects one of an
/// address's connections at a time, and a connection from an address that
/// has been waiting already (open, or closed within the last window without
/// being let in) continues that wait rather than starting a new one. So
/// neither holding many connections nor reopening one keeps a slot.
const LOGIN_GRACE: Duration = Duration::from_secs(30);
/// New connections one address may open in a burst: a page load (the page,
/// its script and style, the icon, the socket) takes five, so a few reloads
/// in a row, or a few devices behind one address, pass.
const ARRIVAL_BURST: f64 = 20.0;
/// After a burst, one more new connection per address this often.
const ARRIVAL_REFILL: Duration = Duration::from_secs(2);
/// New connections all the addresses of one IPv6 `/64` may open in a
/// burst, on top of each address's own [`ARRIVAL_BURST`]: sixteen page
/// loads at once, so a room of tablets on one LAN loading the console
/// together pass, while one machine taking a new address for every
/// connection gets no more than this.
const PREFIX_ARRIVAL_BURST: f64 = 4.0 * ARRIVAL_BURST;
/// After a burst, one more new connection per IPv6 `/64` this often: four
/// times an address's rate, so a few devices reconnecting at once share it
/// and one device on its own never runs it dry.
const PREFIX_ARRIVAL_REFILL: Duration = Duration::from_millis(500);
/// Connections that grace protects at once in one IPv6 `/64`, one per
/// address (see [`Slots::make_room`]): its longest waiting addresses' only.
/// Enough for the operators of one LAN to log in together; one machine
/// taking a new address for every connection pins no more slots than this.
/// The `/64`'s other connections never let in may be closed to make room,
/// as an address's extra ones may.
const MAX_PROTECTED_PER_PREFIX: usize = 8;
/// Addresses (and, apart, IPv6 `/64`s) whose new connections are counted at
/// once. Past this, those that have not connected for a while (their count
/// is back to a full burst) are forgotten first; the memory stays bounded
/// either way.
const MAX_TRACKED_ADDRESSES: usize = 1024;
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

/// The one header a trusted proxy names its client in
/// (`--forwarded-header`). Only that one is read: a proxy passes on what
/// the client sent in the others, and the client could name itself there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ForwardedHeader {
    /// `X-Forwarded-For`, its last address (the one the proxy appended).
    #[default]
    XForwardedFor,
    /// `Forwarded`, the `for=` of its last element.
    Forwarded,
    /// `X-Real-IP`.
    XRealIp,
}

impl ForwardedHeader {
    /// As `--forwarded-header` takes it, any case.
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "x-forwarded-for" => Some(Self::XForwardedFor),
            "forwarded" => Some(Self::Forwarded),
            "x-real-ip" => Some(Self::XRealIp),
            _ => None,
        }
    }
}

/// Reverse proxies whose word on who a client is is taken
/// (`--trusted-proxy`), and where they give it (`--forwarded-header`).
#[derive(Clone, Debug, Default)]
pub struct Proxies {
    pub addrs: Vec<IpAddr>,
    pub header: ForwardedHeader,
}

impl Proxies {
    /// Whether a connection from `socket` comes from one of them.
    fn trusts(&self, socket: Option<IpAddr>) -> bool {
        socket.is_some_and(|s| {
            let s = s.to_canonical();
            self.addrs.iter().any(|a| a.to_canonical() == s)
        })
    }
}

/// What the operator said about how browsers reach this server.
#[derive(Default)]
struct Config {
    /// The names a request's `Host` may carry besides IP addresses,
    /// `localhost` and this machine's own name: `--web-host`'s, lower-cased.
    names: Vec<String>,
    /// Reverse proxies whose forwarding header is believed.
    proxies: Proxies,
}

/// Whether the command loop has let a connection's client in, shared by the
/// connection's thread, the slot table and the command loop.
#[derive(Clone)]
pub struct Admission(Arc<AdmissionState>);

/// Never let in yet, or shut out again.
const WAITING: u8 = 0;
const ADMITTED: u8 = 1;
/// Closed to make room for another connection. Final.
const EVICTED: u8 = 2;

struct AdmissionState {
    /// [`WAITING`], [`ADMITTED`] or [`EVICTED`]. Every move is one
    /// compare-and-swap, so a client let in is never evicted, however the
    /// command loop and the accept thread race.
    state: AtomicU8,
    /// Since when it has been waiting to be let in. Locked while the state
    /// leaves [`ADMITTED`] for [`WAITING`] and while an eviction decides, so
    /// an eviction never reads a stale time for a client just shut out.
    waiting_since: Mutex<Waiting>,
}

/// A connection's wait, on two clocks.
#[derive(Clone, Copy)]
struct Waiting {
    /// Its own wait, for [`LOGIN_DEADLINE`].
    since: Instant,
    /// Where its [`LOGIN_GRACE`] counts from: `since`, or earlier when its
    /// address had been waiting already (see [`Slots::open`]).
    grace: Instant,
}

impl Admission {
    fn new(now: Instant) -> Self {
        Self(Arc::new(AdmissionState {
            state: AtomicU8::new(WAITING),
            waiting_since: Mutex::new(Waiting {
                since: now,
                grace: now,
            }),
        }))
    }

    fn since(&self) -> std::sync::MutexGuard<'_, Waiting> {
        self.0
            .waiting_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn state(&self) -> u8 {
        self.0.state.load(Ordering::Acquire)
    }

    /// The command loop let the client in (`true`) or shut it out (logged
    /// out, its user removed): a client shut out waits again from now, with
    /// a new [`LOGIN_GRACE`]. One already evicted stays so.
    pub fn set(&self, admitted: bool) {
        if admitted {
            let _ = self.0.state.compare_exchange(
                WAITING,
                ADMITTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return;
        }
        let mut since = self.since();
        if self
            .0
            .state
            .compare_exchange(ADMITTED, WAITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let now = Instant::now();
            *since = Waiting {
                since: now,
                grace: now,
            };
        }
    }

    fn waiting(&self) -> bool {
        self.state() == WAITING
    }

    /// Where its [`LOGIN_GRACE`] counts from, while it waits to be let in.
    fn grace_since(&self) -> Option<Instant> {
        let grace = self.since().grace;
        self.waiting().then_some(grace)
    }

    /// Count its [`LOGIN_GRACE`] from `earlier`, if that is earlier: its
    /// address was waiting already.
    fn continue_grace(&self, earlier: Instant) {
        let mut since = self.since();
        since.grace = since.grace.min(earlier);
    }

    /// Mark it evicted if it still may be: `false` when it was let in, or
    /// shut out again (a new grace), meanwhile. `unprotected`: its address
    /// holds another connection that grace protects, so its own grace does
    /// not count.
    fn evict(&self, now: Instant, unprotected: bool) -> bool {
        let since = self.since();
        if !unprotected && now.saturating_duration_since(since.grace) < LOGIN_GRACE {
            return false;
        }
        self.0
            .state
            .compare_exchange(WAITING, EVICTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Evicted, or kept waiting past [`LOGIN_DEADLINE`]: close it.
    fn should_close(&self, now: Instant) -> bool {
        match self.state() {
            EVICTED => true,
            ADMITTED => false,
            _ => now.saturating_duration_since(self.since().since) >= LOGIN_DEADLINE,
        }
    }
}

/// The address limits count by: an IPv4-mapped address is its IPv4
/// address, and an IPv6 address is itself, so devices sharing a LAN's `/64`
/// do not share one address's limits (#338). What one machine gains by
/// taking many addresses in its `/64` is bounded by [`prefix_key`]'s limits.
fn peer_key(peer: IpAddr) -> IpAddr {
    peer.to_canonical()
}

/// The IPv6 `/64` an address is in, whose addresses share a rate of new
/// connections ([`PREFIX_ARRIVAL_BURST`]) and [`MAX_PROTECTED_PER_PREFIX`]
/// on top of each address's own limits: one machine can take as many
/// addresses in its `/64` as it likes (#266). `None` for IPv4 (and
/// IPv4-mapped) and loopback, which have no such level.
fn prefix_key(peer: IpAddr) -> Option<Ipv6Addr> {
    match peer.to_canonical() {
        IpAddr::V6(v6) if !v6.is_loopback() => {
            let [a, b, c, d, ..] = v6.segments();
            Some(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
        }
        _ => None,
    }
}

/// An address held to one [`LOGIN_GRACE`] (see [`Slots`]): known, and not
/// this machine (a browser here, or a proxy in front, which many share).
fn grouped(peer: Option<IpAddr>) -> Option<IpAddr> {
    peer.filter(|p| !p.is_loopback())
}

/// One open connection, as the slot table keeps it.
struct Slot {
    id: ClientId,
    /// As [`peer_key`] gives it; `None` when the address could not be read.
    peer: Option<IpAddr>,
    admission: Admission,
    /// To close it from the accept thread when it is evicted.
    stream: Option<TcpStream>,
}

/// Every open connection: the one place that decides who gets a slot.
#[derive(Default)]
struct Slots {
    open: Vec<Slot>,
    /// Addresses whose connections closed without being let in, within the
    /// last [`LOGIN_GRACE`]: a new connection from one continues their wait
    /// (see [`Self::open`]). At most [`MAX_TRACKED_ADDRESSES`].
    closed: HashMap<IpAddr, Closed>,
}

/// An address's connections that closed without being let in.
#[derive(Clone, Copy)]
struct Closed {
    /// The earliest grace among them.
    grace: Instant,
    /// When the last one closed.
    at: Instant,
}

type SharedSlots = Arc<Mutex<Slots>>;

impl Slots {
    /// Take a slot for connection `id` from `peer`; `false` when there is no
    /// room. A peer other than this machine (a browser here, or a proxy in
    /// front, is never limited this way) holds at most [`MAX_PER_PEER`]
    /// never let in. Its new connection continues the wait of those it
    /// holds, or closed within the last [`LOGIN_GRACE`], rather than
    /// starting a grace of its own: reopening a connection, or replacing an
    /// old one with it, never keeps a slot safe. When every slot is taken,
    /// room is made as [`Self::make_room`] says.
    fn open(
        &mut self,
        id: ClientId,
        peer: Option<IpAddr>,
        now: Instant,
        admission: Admission,
        stream: Option<TcpStream>,
    ) -> bool {
        let peer = peer.map(peer_key);
        if !self.join_peer(id, peer, now, &admission) {
            return false;
        }
        if self.open.len() >= MAX_CONNECTIONS && !self.make_room(peer, now) {
            return false;
        }
        self.open.push(Slot {
            id,
            peer,
            admission,
            stream,
        });
        true
    }

    /// Count connection `id` under `peer` (as [`peer_key`] gives it): `false`
    /// when that address holds [`MAX_PER_PEER`] never let in already. Its
    /// grace continues that address's wait (see [`Self::open`]).
    fn join_peer(
        &self,
        id: ClientId,
        peer: Option<IpAddr>,
        now: Instant,
        admission: &Admission,
    ) -> bool {
        let Some(key) = grouped(peer) else {
            return true;
        };
        let others = || {
            self.open
                .iter()
                .filter(move |s| s.id != id && s.peer == Some(key))
        };
        let held = others().filter(|s| s.admission.waiting()).count();
        if held >= MAX_PER_PEER {
            return false;
        }
        let closed = self
            .closed
            .get(&key)
            .filter(|c| now.saturating_duration_since(c.at) < LOGIN_GRACE)
            .map(|c| c.grace);
        let earliest = others()
            .filter_map(|s| s.admission.grace_since())
            .chain(closed)
            .min();
        if let Some(earliest) = earliest {
            admission.continue_grace(earliest);
        }
        true
    }

    /// Connection `id`, taken under its proxy's address, is from `client`
    /// (as a trusted proxy forwarded it): count it under that address from
    /// now on, as if it had come from there. `false`: it may not stay (that
    /// address holds too many already, or it was closed meanwhile).
    fn forwarded(&mut self, id: ClientId, client: IpAddr, now: Instant) -> bool {
        let Some(at) = self.open.iter().position(|slot| slot.id == id) else {
            return false;
        };
        let key = Some(peer_key(client));
        let admission = self.open[at].admission.clone();
        if !self.join_peer(id, key, now, &admission) {
            return false;
        }
        self.open[at].peer = key;
        true
    }

    /// Close one connection to make room for a new one from `peer` (as
    /// [`peer_key`] gives it); `false` when none may be closed, and the new
    /// one is turned away instead.
    ///
    /// - Never one let in (logged in, or past the setup code).
    /// - Never one inside its [`LOGIN_GRACE`] that is the only one its
    ///   address has there (its longest waiting, when it has more): a fresh
    ///   connection, an operator's about to log in among them, cannot be
    ///   pushed out however many connections arrive. An address's others
    ///   are not protected, so a few addresses holding many connections
    ///   cannot keep the pool full: only as many addresses as there are
    ///   slots, each holding one, turn newcomers away. An IPv6 `/64` keeps
    ///   that protection for its [`MAX_PROTECTED_PER_PREFIX`] longest
    ///   waiting addresses only (see [`prefix_key`]): one machine taking
    ///   a new address for each connection pins no more than that.
    /// - When `peer` itself holds connections past their grace, its own
    ///   longest waiting is the one replaced: reconnecting from one address
    ///   never pushes out somebody else's.
    /// - Otherwise, from the address with the most connections never let
    ///   in, its longest waiting: a few addresses holding many lose theirs
    ///   before one holding a single connection does.
    ///
    /// Somebody else's connection closed here gets a new grace when its page
    /// reconnects: what its address had closed before is forgotten.
    fn make_room(&mut self, peer: Option<IpAddr>, now: Instant) -> bool {
        let in_grace = |grace: Instant| now.saturating_duration_since(grace) < LOGIN_GRACE;
        let mut waiting: HashMap<Option<IpAddr>, usize> = HashMap::new();
        // Per grouped address, the one connection grace protects.
        let mut protected: HashMap<IpAddr, (Instant, ClientId)> = HashMap::new();
        for slot in &self.open {
            let Some(grace) = slot.admission.grace_since() else {
                continue;
            };
            *waiting.entry(slot.peer).or_insert(0) += 1;
            if let Some(key) = grouped(slot.peer).filter(|_| in_grace(grace)) {
                let first = protected.entry(key).or_insert((grace, slot.id));
                *first = (*first).min((grace, slot.id));
            }
        }
        // Per IPv6 /64, only its longest waiting addresses' keep it. At
        // most one entry per open slot, so this stays as small as the pool.
        let mut by_prefix: Vec<(Ipv6Addr, Instant, ClientId, IpAddr)> = protected
            .iter()
            .filter_map(|(&key, &(grace, id))| Some((prefix_key(key)?, grace, id, key)))
            .collect();
        by_prefix.sort_unstable();
        let (mut last, mut count) = (None, 0);
        for (prefix, _, _, key) in by_prefix {
            if last != Some(prefix) {
                (last, count) = (Some(prefix), 0);
            }
            count += 1;
            if count > MAX_PROTECTED_PER_PREFIX {
                protected.remove(&key);
            }
        }
        // (index, own and past its grace, held by its address, grace since,
        // unprotected)
        let mut candidates: Vec<(usize, bool, usize, Instant, bool)> = self
            .open
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let grace = slot.admission.grace_since()?;
                // Inside its grace, but not the one its address (or /64)
                // keeps protected.
                let unprotected = in_grace(grace)
                    && grouped(slot.peer).is_some_and(|key| {
                        protected.get(&key).map(|&(_, first)| first) != Some(slot.id)
                    });
                if in_grace(grace) && !unprotected {
                    return None;
                }
                let own_stale = !unprotected && slot.peer == peer;
                let held = waiting.get(&slot.peer).copied().unwrap_or(0);
                Some((i, own_stale, held, grace, unprotected))
            })
            .collect();
        if candidates.iter().any(|&(_, own, ..)| own) {
            candidates.retain(|&(_, own, ..)| own);
        }
        candidates.sort_by(|a, b| b.2.cmp(&a.2).then(a.3.cmp(&b.3)));
        // In order: one let in since the list was made is skipped.
        for (i, _, _, _, unprotected) in candidates {
            if self.open[i].admission.evict(now, unprotected) {
                let slot = self.open.swap_remove(i);
                if let Some(key) = grouped(slot.peer).filter(|_| slot.peer != peer) {
                    self.closed.remove(&key);
                }
                if let Some(stream) = &slot.stream {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                return true;
            }
        }
        false
    }

    /// Connection `id` ended at `now`. One never let in is remembered for
    /// its address (see [`Self::open`]).
    fn close(&mut self, id: ClientId, now: Instant) {
        let Some(at) = self.open.iter().position(|slot| slot.id == id) else {
            return;
        };
        let slot = self.open.swap_remove(at);
        let (Some(key), Some(grace)) = (grouped(slot.peer), slot.admission.grace_since()) else {
            return;
        };
        let fresh = |c: &Closed| now.saturating_duration_since(c.at) < LOGIN_GRACE;
        if !self.closed.contains_key(&key) && self.closed.len() >= MAX_TRACKED_ADDRESSES {
            self.closed.retain(|_, c| fresh(c));
            if self.closed.len() >= MAX_TRACKED_ADDRESSES {
                let oldest = self
                    .closed
                    .iter()
                    .min_by_key(|(_, c)| c.at)
                    .map(|(k, _)| *k);
                if let Some(oldest) = oldest {
                    self.closed.remove(&oldest);
                }
            }
        }
        let entry = self.closed.entry(key).or_insert(Closed { grace, at: now });
        if fresh(entry) {
            entry.grace = entry.grace.min(grace);
            entry.at = entry.at.max(now);
        } else {
            *entry = Closed { grace, at: now };
        }
    }
}

/// How fast each address opens new connections: a token bucket per address
/// ([`ARRIVAL_BURST`], one more every [`ARRIVAL_REFILL`]) and, on top, one
/// per IPv6 `/64` for all its addresses together ([`PREFIX_ARRIVAL_BURST`],
/// one more every [`PREFIX_ARRIVAL_REFILL`]); a new connection needs a
/// token from both. At most [`MAX_TRACKED_ADDRESSES`] of each are kept.
/// This machine is never limited (a browser here, or a proxy in front).
/// Only the accept thread uses it.
#[derive(Default)]
struct Arrivals {
    /// Per address, as [`peer_key`] gives it.
    buckets: HashMap<Option<IpAddr>, Bucket>,
    /// Per IPv6 `/64`, as [`prefix_key`] gives it.
    prefixes: HashMap<Ipv6Addr, Bucket>,
}

/// How full a [`Bucket`] gets, and how fast it fills.
#[derive(Clone, Copy)]
struct Rate {
    burst: f64,
    refill: Duration,
}

/// An address's own rate.
const ADDRESS_RATE: Rate = Rate {
    burst: ARRIVAL_BURST,
    refill: ARRIVAL_REFILL,
};
/// An IPv6 `/64`'s, all its addresses together.
const PREFIX_RATE: Rate = Rate {
    burst: PREFIX_ARRIVAL_BURST,
    refill: PREFIX_ARRIVAL_REFILL,
};

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn level(&self, now: Instant, rate: Rate) -> f64 {
        let refilled =
            now.saturating_duration_since(self.at).as_secs_f64() / rate.refill.as_secs_f64();
        (self.tokens + refilled).min(rate.burst)
    }

    /// Bring it up to `now`; how many tokens it holds.
    fn settle(&mut self, now: Instant, rate: Rate) -> f64 {
        self.tokens = self.level(now, rate);
        self.at = now.max(self.at);
        self.tokens
    }
}

/// `key`'s bucket in `buckets`, a full one when it has none; making room
/// for it first when [`MAX_TRACKED_ADDRESSES`] are kept already: those whose
/// bucket is full again (the same as never seen) are forgotten; failing
/// that, the one with the most left, which is the closest to it. One being
/// limited is the last one forgotten.
fn bucket<K: Copy + Eq + std::hash::Hash>(
    buckets: &mut HashMap<K, Bucket>,
    key: K,
    rate: Rate,
    now: Instant,
) -> &mut Bucket {
    if !buckets.contains_key(&key) && buckets.len() >= MAX_TRACKED_ADDRESSES {
        buckets.retain(|_, b| b.level(now, rate) < rate.burst);
        if buckets.len() >= MAX_TRACKED_ADDRESSES {
            let fullest = buckets
                .iter()
                .max_by(|a, b| a.1.level(now, rate).total_cmp(&b.1.level(now, rate)))
                .map(|(key, _)| *key);
            if let Some(fullest) = fullest {
                buckets.remove(&fullest);
            }
        }
    }
    buckets.entry(key).or_insert(Bucket {
        tokens: rate.burst,
        at: now,
    })
}

impl Arrivals {
    /// Count a new connection from `peer` (see [`peer_key`]); `false`: too
    /// many lately, from it or from its IPv6 `/64`, turn it away. It takes
    /// a token from both or from neither: one level turning it away does not
    /// spend the other's.
    fn allow(&mut self, peer: Option<IpAddr>, now: Instant) -> bool {
        if peer.is_some_and(|p| p.is_loopback()) {
            return true;
        }
        let own = bucket(&mut self.buckets, peer, ADDRESS_RATE, now);
        let mut room = own.settle(now, ADDRESS_RATE) >= 1.0;
        let mut shared = peer
            .and_then(prefix_key)
            .map(|prefix| bucket(&mut self.prefixes, prefix, PREFIX_RATE, now));
        if let Some(shared) = shared.as_deref_mut() {
            room &= shared.settle(now, PREFIX_RATE) >= 1.0;
        }
        if !room {
            return false;
        }
        own.tokens -= 1.0;
        if let Some(shared) = shared {
            shared.tokens -= 1.0;
        }
        true
    }
}

/// What became of a new connection.
#[derive(Debug, PartialEq, Eq)]
enum Arrival {
    /// It holds a slot.
    Taken,
    /// No slot free, and none that may be made free.
    Busy,
    /// Its address opened too many lately.
    TooFast,
}

impl Arrival {
    /// The answer for one turned away.
    fn refusal(&self) -> Option<(u16, &'static str, &'static [u8])> {
        match self {
            Arrival::Taken => None,
            Arrival::Busy => Some((503, "Service Unavailable", b"busy")),
            Arrival::TooFast => Some((429, "Too Many Requests", b"too many connections")),
        }
    }
}

type SharedArrivals = Arc<Mutex<Arrivals>>;

/// The accept thread's limits, for a connection that turns out to come
/// through a trusted proxy (see [`forwarded`]).
#[derive(Clone, Copy)]
struct Limits<'a> {
    slots: &'a SharedSlots,
    arrivals: &'a SharedArrivals,
}

/// Count connection `id` under the `client` a trusted proxy forwarded it
/// for, as [`admit`] would have had it connected directly: its rate first,
/// then its share of the slots. Taken under the proxy's address (this
/// machine's, most often), it was let through both.
fn forwarded(limits: Limits<'_>, id: ClientId, client: IpAddr, now: Instant) -> Arrival {
    let key = Some(peer_key(client));
    if !limits
        .arrivals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(key, now)
    {
        return Arrival::TooFast;
    }
    let kept = limits
        .slots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forwarded(id, client, now);
    if kept {
        Arrival::Taken
    } else {
        Arrival::Busy
    }
}

/// Admit a new connection: its address's rate first, then a slot.
fn admit(
    slots: &SharedSlots,
    arrivals: &mut Arrivals,
    id: ClientId,
    peer: Option<IpAddr>,
    now: Instant,
    admission: Admission,
    stream: Option<TcpStream>,
) -> Arrival {
    let peer = peer.map(peer_key);
    if !arrivals.allow(peer, now) {
        return Arrival::TooFast;
    }
    let taken = slots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open(id, peer, now, admission, stream);
    if taken {
        Arrival::Taken
    } else {
        Arrival::Busy
    }
}

/// Admit a new connection from a trusted proxy: held to no address's
/// limits (the proxy speaks for many clients), as one from this machine is
/// not; once its request head names the client, [`forwarded`] counts it
/// under that client's. It still needs a slot.
fn admit_proxied(
    slots: &SharedSlots,
    id: ClientId,
    now: Instant,
    admission: Admission,
    stream: Option<TcpStream>,
) -> Arrival {
    let taken = slots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open(id, None, now, admission, stream);
    if taken {
        Arrival::Taken
    } else {
        Arrival::Busy
    }
}

/// Admit a new connection from `socket` on the accept thread: as
/// [`admit_proxied`] says when a trusted proxy opened it, else as [`admit`].
fn accept(
    limits: Limits<'_>,
    proxies: &Proxies,
    id: ClientId,
    socket: Option<IpAddr>,
    now: Instant,
    admission: Admission,
    stream: Option<TcpStream>,
) -> Arrival {
    if proxies.trusts(socket) {
        return admit_proxied(limits.slots, id, now, admission, stream);
    }
    admit(
        limits.slots,
        &mut limits.arrivals.lock().unwrap_or_else(|e| e.into_inner()),
        id,
        socket,
        now,
        admission,
        stream,
    )
}

/// Listen on `addr` and serve until the process ends. Returns the address
/// actually bound (port 0 picks one). `names`: what `--web-host` added to
/// the names a request may reach this server by. `proxies`: the reverse
/// proxies whose forwarding header says who a client is
/// (`--trusted-proxy`, `--forwarded-header`).
pub fn serve(
    addr: SocketAddr,
    inbound: Sender<Inbound>,
    names: Vec<String>,
    proxies: Proxies,
) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let bound = listener.local_addr()?;
    let config = Arc::new(Config { names, proxies });
    let slots: SharedSlots = Arc::default();
    let arrivals: SharedArrivals = Arc::default();
    std::thread::Builder::new()
        .name("livestage-web".to_string())
        .spawn(move || {
            let mut next_id: ClientId = 1;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    continue;
                };
                let id = next_id;
                next_id += 1;
                let now = Instant::now();
                let admission = Admission::new(now);
                let socket = stream.peer_addr().ok().map(|a| a.ip());
                let limits = Limits {
                    slots: &slots,
                    arrivals: &arrivals,
                };
                let arrival = accept(
                    limits,
                    &config.proxies,
                    id,
                    socket,
                    now,
                    admission.clone(),
                    stream.try_clone().ok(),
                );
                if let Some((code, reason, body)) = arrival.refusal() {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    let _ = respond(&mut stream, code, reason, &[("Retry-After", "2")], body);
                    continue;
                }
                let inbound = inbound.clone();
                let config = config.clone();
                let spawned = std::thread::Builder::new()
                    .name("livestage-web-conn".to_string())
                    .spawn({
                        let slots = slots.clone();
                        let arrivals = arrivals.clone();
                        move || {
                            let limits = Limits {
                                slots: &slots,
                                arrivals: &arrivals,
                            };
                            connection(stream, id, &inbound, &config, limits, admission);
                            slots
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .close(id, Instant::now());
                        }
                    });
                if spawned.is_err() {
                    slots
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .close(id, Instant::now());
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

/// Who sent `request` over a connection from `socket` (see the module
/// notes), and whether a trusted proxy said so (the address it forwarded
/// is then the [`Peer::addr`]).
///
/// - From a proxy in `proxies`: the client its forwarding header names;
///   never this machine, as the proxy may be anywhere. Without that header
///   it is the proxy itself, and not taken for this machine either.
/// - Otherwise the connection's own address; this machine only when that
///   is loopback, the request names this machine, no forwarding header
///   says a proxy sent it, and the server is not
///   [`reached_from_elsewhere`] (#351).
fn client_peer(
    socket: Option<IpAddr>,
    request: &Request,
    names: &[String],
    proxies: &Proxies,
) -> (Peer, bool) {
    if proxies.trusts(socket) {
        return match forwarded_client(request, proxies.header) {
            Some(client) => (
                Peer {
                    addr: Some(client),
                    local: false,
                },
                true,
            ),
            None => (
                Peer {
                    addr: socket,
                    local: false,
                },
                false,
            ),
        };
    }
    let proxied = FORWARDING_HEADERS
        .iter()
        .any(|name| request.header(name).is_some());
    let local = !reached_from_elsewhere(names, proxies)
        && socket.is_some_and(|s| s.to_canonical().is_loopback())
        && !proxied
        && request.header("host").is_some_and(names_this_machine);
    (
        Peer {
            addr: socket,
            local,
        },
        false,
    )
}

/// Whether the operator said browsers reach this server from elsewhere
/// through a name or a proxy: a `--web-host` (`names`) that does not name
/// this machine only, or any `--trusted-proxy`. A proxy here may then pass a
/// remote client's `Host: localhost` on with no forwarding header, so no
/// connection is taken for a browser on this machine, and every browser
/// needs the setup code while there are no users (#351).
pub fn reached_from_elsewhere(names: &[String], proxies: &Proxies) -> bool {
    !proxies.addrs.is_empty() || names.iter().any(|name| !names_this_machine(name))
}

/// Whether a `Host` names this machine only: `localhost` (or a name under
/// it) or a loopback address. A reverse proxy passes on the name the
/// browser used, which is the proxy's.
fn names_this_machine(host: &str) -> bool {
    let Some(host) = host_name(host) else {
        return false;
    };
    host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// The client a proxy forwarded `request` for, from `header` only (see
/// [`ForwardedHeader`]): the last address of `X-Forwarded-For` (the one
/// the proxy appended; those before it are what the client claimed), the
/// last `for=` of `Forwarded`, or `X-Real-IP`. `None` when it holds no
/// address.
fn forwarded_client(request: &Request, header: ForwardedHeader) -> Option<IpAddr> {
    match header {
        ForwardedHeader::XForwardedFor => request
            .header("x-forwarded-for")?
            .rsplit(',')
            .next()
            .and_then(forwarded_address),
        ForwardedHeader::Forwarded => request
            .header("forwarded")?
            .rsplit(',')
            .next()?
            .split(';')
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("for"))
            .and_then(|(_, value)| forwarded_address(value)),
        ForwardedHeader::XRealIp => request.header("x-real-ip").and_then(forwarded_address),
    }
}

/// One forwarded address: `192.0.2.1`, `192.0.2.1:4711`, `2001:db8::1`,
/// `[2001:db8::1]:4711`, quoted or not. `None` for anything else (`unknown`,
/// an obfuscated name).
fn forwarded_address(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"').trim();
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Some(ip.to_canonical());
    }
    let host = match value.strip_prefix('[') {
        Some(rest) => rest.split_once(']')?.0,
        None => value.rsplit_once(':')?.0,
    };
    host.parse::<IpAddr>().ok().map(|ip| ip.to_canonical())
}

const WRONG_HOST_PAGE: &[u8] =
    b"This LiveStage server does not answer to that name. Open it by its \
IP address or NAME.local, or start the server with --web-host NAME for this name.";

fn connection(
    mut stream: TcpStream,
    id: ClientId,
    inbound: &Sender<Inbound>,
    config: &Config,
    limits: Limits<'_>,
    admission: Admission,
) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_nodelay(true);
    let Some((bytes, head_len)) = read_head(&mut stream, Instant::now() + HEAD_DEADLINE) else {
        return;
    };
    let Some(request) = Request::parse(&bytes[..head_len]) else {
        let _ = respond(&mut stream, 400, "Bad Request", &[], b"bad request");
        return;
    };
    // Asked for by name each time: the appliance's setup may rename it.
    if !host_allowed(
        request.header("host"),
        &config.names,
        machine_name().as_deref(),
    ) {
        let _ = respond(
            &mut stream,
            421,
            "Misdirected Request",
            &[("Content-Type", "text/plain; charset=utf-8")],
            WRONG_HOST_PAGE,
        );
        return;
    }
    let socket = stream.peer_addr().ok().map(|a| a.ip());
    let (peer, through_proxy) = client_peer(socket, &request, &config.names, &config.proxies);
    if let (true, Some(client)) = (through_proxy, peer.addr) {
        let arrival = forwarded(limits, id, client, Instant::now());
        if let Some((code, reason, body)) = arrival.refusal() {
            let _ = respond(&mut stream, code, reason, &[("Retry-After", "2")], body);
            return;
        }
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
            peer,
            inbound,
            admission,
        );
    } else {
        let _ = serve_asset(&mut stream, &request);
    }
}

/// Read until the end of the request head, all of it by `until`. Returns
/// everything read and where the head ends.
fn read_head(stream: &mut TcpStream, until: Instant) -> Option<(Vec<u8>, usize)> {
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
    let csp = content_security_policy(request.header("host"));
    let etag = asset.etag.map(|tag| format!("\"{tag}\""));
    if let (Some(etag), Some(wanted)) = (&etag, request.header("if-none-match")) {
        if wanted == etag {
            let headers = [("ETag", etag.as_str()), (CSP_HEADER, csp.as_str())];
            return respond(stream, 304, "Not Modified", &headers, b"");
        }
    }
    // Vite names bundles by content: those never change under their name.
    let cache = if asset.path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut headers = vec![
        ("Content-Type", asset.mime_type),
        ("Cache-Control", cache),
        (CSP_HEADER, csp.as_str()),
    ];
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

const CSP_HEADER: &str = "Content-Security-Policy";

/// What a page from this server may load and run: only this server's own
/// script files (no inline script, no `eval`), so a script that ever got
/// into the page some other way does not run; and never inside another
/// site's frame, where a page could trick a click onto the mixer. Styles
/// may be inline (the not-built page has some); pictures and fonts Vite
/// inlined come as `data:`.
const BASE_CSP: &str = "default-src 'self'; script-src 'self'; \
style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; \
object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// [`BASE_CSP`], and where the page may connect: this server, its
/// WebSocket among it. `'self'` covers `ws:` only in newer browsers, so the
/// socket's own address is named too when the request's `Host` is a plain
/// `name:port` (it has passed [`host_allowed`] by then); anything else in
/// it is left out rather than written into the policy.
fn content_security_policy(host: Option<&str>) -> String {
    let host = host.map(str::trim).filter(|h| {
        !h.is_empty()
            && h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
    });
    match host {
        Some(host) => format!("{BASE_CSP}; connect-src 'self' ws://{host} wss://{host}"),
        None => format!("{BASE_CSP}; connect-src 'self'"),
    }
}

/// Writes the status line, `headers`, and the headers every answer
/// carries; a [`CSP_HEADER`] among `headers` takes the place of the
/// default one.
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
    if !headers.iter().any(|(name, _)| *name == CSP_HEADER) {
        head.push_str(&format!(
            "{CSP_HEADER}: {}\r\n",
            content_security_policy(None)
        ));
    }
    head.push_str(
        "X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nConnection: close\r\n\r\n",
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

fn websocket(
    stream: Replay,
    id: ClientId,
    peer: Peer,
    inbound: &Sender<Inbound>,
    admission: Admission,
) {
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
    // Failed log-ins are counted per address (see `client_peer`).
    if inbound
        .send(Inbound::Joined(id, outbox, peer, admission.clone()))
        .is_err()
    {
        return;
    }
    'connection: loop {
        // Never let in in time, or closed to make room for another.
        if admission.should_close(Instant::now()) {
            break;
        }
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

    /// Open `count` connections from `peer` into `slots`, ids from `first`.
    fn fill(
        slots: &mut Slots,
        first: ClientId,
        count: usize,
        peer: &str,
        now: Instant,
    ) -> Vec<Admission> {
        (0..count)
            .map(|n| {
                let admission = Admission::new(now);
                assert!(slots.open(
                    first + n as ClientId,
                    Some(peer.parse().unwrap()),
                    now,
                    admission.clone(),
                    None,
                ));
                admission
            })
            .collect()
    }

    #[test]
    fn one_address_cannot_take_every_connection() {
        let mut slots = Slots::default();
        let now = Instant::now();
        let peer: IpAddr = "10.0.0.9".parse().unwrap();
        fill(&mut slots, 1, MAX_PER_PEER, "10.0.0.9", now);
        assert!(!slots.open(100, Some(peer), now, Admission::new(now), None));
        assert!(slots.open(
            101,
            Some("10.0.0.10".parse().unwrap()),
            now,
            Admission::new(now),
            None
        ));
        // This machine is never limited.
        fill(&mut slots, 200, MAX_PER_PEER + 1, "127.0.0.1", now);
        slots.close(1, now);
        assert!(slots.open(102, Some(peer), now, Admission::new(now), None));
        // Only those never let in count: devices logged in behind one
        // address do not keep another from connecting.
        let mut slots = Slots::default();
        for admission in fill(&mut slots, 1, MAX_PER_PEER, "10.0.0.9", now) {
            admission.set(true);
        }
        fill(&mut slots, 100, MAX_PER_PEER, "10.0.0.9", now);
        assert!(!slots.open(200, Some(peer), now, Admission::new(now), None));
    }

    /// Machines that connect and never log in fill every slot. An operator
    /// gets in even while they are inside their grace, as grace protects
    /// one connection per address; once their grace is over, by closing the
    /// oldest connection of the peer holding the most that were never let
    /// in — and one let in is never closed.
    #[test]
    fn idle_connections_make_room_for_an_operator() {
        let mut slots = Slots::default();
        let start = Instant::now();
        let operator = fill(&mut slots, 1, 1, "10.0.0.50", start);
        operator[0].set(true);
        let mut idle = Vec::new();
        for (n, peer) in ["10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4"]
            .iter()
            .enumerate()
        {
            let at = start + Duration::from_millis(n as u64 + 1);
            let count = if n == 3 {
                MAX_CONNECTIONS - 1 - 3 * MAX_PER_PEER
            } else {
                MAX_PER_PEER
            };
            idle.extend(fill(&mut slots, 10 + 100 * n as ClientId, count, peer, at));
        }
        assert_eq!(slots.open.len(), MAX_CONNECTIONS);

        // While all of them are inside their grace, a newcomer still gets
        // in: four addresses hold them, and grace protects one each. One of
        // the first address's others is closed (it holds the most, and its
        // connections are the oldest of those holding as many).
        let early = start + LOGIN_GRACE - Duration::from_secs(1);
        let early_one = Admission::new(early);
        let early_peer = Some("10.0.0.52".parse().unwrap());
        assert!(slots.open(998, early_peer, early, early_one.clone(), None));
        let closed_early = |range: std::ops::Range<usize>| {
            idle[range].iter().filter(|a| a.should_close(early)).count()
        };
        assert_eq!(closed_early(0..MAX_PER_PEER), 1);
        assert_eq!(closed_early(0..idle.len()), 1, "exactly one closed");
        // Never the one grace protects: the address's longest waiting.
        assert!(!idle[0].should_close(early));

        // An operator reconnecting (not logged in yet on the new socket).
        let later = start + LOGIN_GRACE + Duration::from_secs(1);
        let reconnect = Admission::new(later);
        assert!(slots.open(
            999,
            Some("10.0.0.51".parse().unwrap()),
            later,
            reconnect.clone(),
            None
        ));
        assert_eq!(slots.open.len(), MAX_CONNECTIONS);
        // The victim: one of the second peer's (the first now holds one
        // fewer; the second's are older than the third's).
        assert_eq!(
            idle[MAX_PER_PEER..2 * MAX_PER_PEER]
                .iter()
                .filter(|a| a.should_close(later))
                .count(),
            1
        );
        assert_eq!(
            idle.iter().filter(|a| a.should_close(later)).count(),
            2,
            "one more closed"
        );
        assert!(!operator[0].should_close(later));
        assert!(!early_one.should_close(later));
        assert!(slots.open.iter().any(|s| s.id == 998));
        assert!(slots.open.iter().any(|s| s.id == 999));

        // As many addresses as slots, each holding one fresh connection:
        // grace protects them all, and a newcomer is turned away.
        let mut singles = Slots::default();
        for n in 0..MAX_CONNECTIONS {
            fill(
                &mut singles,
                1 + n as ClientId,
                1,
                &address(n).to_string(),
                start,
            );
        }
        assert!(!singles.open(
            600,
            Some("10.0.0.99".parse().unwrap()),
            early,
            Admission::new(early),
            None
        ));

        // When every slot is someone let in, a new one is turned away.
        let mut full = Slots::default();
        for admission in fill(&mut full, 1, MAX_CONNECTIONS, "127.0.0.1", start) {
            admission.set(true);
        }
        assert!(!full.open(
            500,
            Some("10.0.0.8".parse().unwrap()),
            later,
            Admission::new(later),
            None
        ));
    }

    #[test]
    fn a_connection_never_let_in_is_closed_after_the_login_deadline() {
        let start = Instant::now();
        let waiting = Admission::new(start);
        assert!(!waiting.should_close(start + LOGIN_DEADLINE - Duration::from_secs(1)));
        assert!(waiting.should_close(start + LOGIN_DEADLINE));
        let admitted = Admission::new(start);
        admitted.set(true);
        assert!(!admitted.should_close(start + LOGIN_DEADLINE * 10));
        // Shut out again (logged out): it waits from now, not from its start.
        admitted.set(false);
        assert!(!admitted.should_close(Instant::now() + Duration::from_secs(1)));
    }

    fn address(n: usize) -> IpAddr {
        IpAddr::from([10, 1, (n / 200) as u8, (n % 200 + 1) as u8])
    }

    fn evicted(admission: &Admission) -> bool {
        admission.state() == EVICTED
    }

    /// Who is closed to make room: an address's own stale connection
    /// first, else the stale one of the address holding the most; never a
    /// connection inside its grace, never one let in.
    #[test]
    fn eviction_takes_own_then_the_biggest_holder_never_fresh_or_admitted() {
        let mut slots = Slots::default();
        let start = Instant::now();
        let fresh_at = start + LOGIN_GRACE;
        let many = fill(&mut slots, 1, 10, "10.0.0.1", start);
        let lone = fill(&mut slots, 100, 1, "10.0.0.2", start);
        let user = fill(&mut slots, 200, 1, "10.0.0.3", start);
        user[0].set(true);
        let operator = fill(&mut slots, 300, 1, "10.0.0.4", fresh_at);
        let mut fresh = Vec::new();
        for n in 0..MAX_CONNECTIONS - 13 {
            fresh.extend(fill(
                &mut slots,
                400 + n as ClientId,
                1,
                &address(n).to_string(),
                fresh_at,
            ));
        }
        assert_eq!(slots.open.len(), MAX_CONNECTIONS);
        let now = fresh_at + Duration::from_secs(1);

        // The lone address reconnects: its own stale connection goes, not
        // one of the address holding ten. The new one continues its wait:
        // replacing a connection does not buy a new grace.
        let peer = Some("10.0.0.2".parse().unwrap());
        let reconnect = Admission::new(now);
        assert!(slots.open(1000, peer, now, reconnect.clone(), None));
        assert!(evicted(&lone[0]));
        assert!(many.iter().all(|a| !evicted(a)));
        assert_eq!(reconnect.grace_since(), Some(start));

        // A new address: the oldest of the address holding the most.
        let peer = Some("10.0.0.5".parse().unwrap());
        assert!(slots.open(1001, peer, now, Admission::new(now), None));
        assert!(evicted(&many[0]));
        assert_eq!(many.iter().filter(|a| evicted(a)).count(), 1);

        // Its other nine go next, and the lone address's reconnect, then
        // nobody: the rest are fresh or let in.
        for n in 0..10 {
            let peer = Some(address(500 + n));
            assert!(slots.open(1002 + n as ClientId, peer, now, Admission::new(now), None));
        }
        assert!(many.iter().all(evicted));
        assert!(evicted(&reconnect));
        let peer = Some(address(600));
        assert!(!slots.open(2000, peer, now, Admission::new(now), None));
        assert!(!evicted(&operator[0]) && !operator[0].should_close(now));
        assert!(!evicted(&user[0]) && !user[0].should_close(now + LOGIN_DEADLINE));
        assert!(fresh.iter().all(|a| !evicted(a)));
        assert_eq!(slots.open.len(), MAX_CONNECTIONS);

        // Once the operator's grace is over without a log-in, it may go.
        let much_later = now + LOGIN_GRACE;
        let peer = Some(address(601));
        assert!(slots.open(2001, peer, much_later, Admission::new(much_later), None));
        assert!(!evicted(&user[0]));
    }

    /// A simulated address in a flood, with the connection it holds.
    struct Attacker {
        peer: IpAddr,
        held: Option<(ClientId, Admission)>,
    }

    /// Addresses that each keep one connection that never logs in, and
    /// reconnect as fast as they are let.
    struct Flood {
        attackers: Vec<Attacker>,
        arrivals: Arrivals,
        next: ClientId,
        taken: usize,
        busy: usize,
        too_fast: usize,
    }

    impl Flood {
        fn new(count: usize) -> Self {
            Self {
                attackers: (0..count)
                    .map(|n| Attacker {
                        peer: address(n),
                        held: None,
                    })
                    .collect(),
                arrivals: Arrivals::default(),
                next: 10_000,
                taken: 0,
                busy: 0,
                too_fast: 0,
            }
        }

        fn open(
            &mut self,
            slots: &SharedSlots,
            n: usize,
            at: Instant,
        ) -> Option<(ClientId, Admission)> {
            let id = self.next;
            self.next += 1;
            let admission = Admission::new(at);
            let peer = Some(self.attackers[n].peer);
            match admit(
                slots,
                &mut self.arrivals,
                id,
                peer,
                at,
                admission.clone(),
                None,
            ) {
                Arrival::Taken => {
                    self.taken += 1;
                    Some((id, admission))
                }
                Arrival::Busy => {
                    self.busy += 1;
                    None
                }
                Arrival::TooFast => {
                    self.too_fast += 1;
                    None
                }
            }
        }

        /// Attacker `n` reconnects: closing its connection first, or (odd
        /// ones) opening the new one first.
        fn reconnect(&mut self, slots: &SharedSlots, n: usize, at: Instant) {
            let close = |id: ClientId| slots.lock().unwrap().close(id, at);
            // A connection closed to make room ends; so does one timed out.
            let old = self.attackers[n].held.take().filter(|(id, admission)| {
                let ended = admission.should_close(at);
                if ended {
                    close(*id);
                }
                !ended
            });
            if n % 2 == 0 {
                if let Some((id, _)) = old {
                    close(id);
                }
                self.attackers[n].held = self.open(slots, n, at);
            } else {
                let new = self.open(slots, n, at);
                self.attackers[n].held = match new {
                    Some(new) => {
                        if let Some((id, _)) = old {
                            close(id);
                        }
                        Some(new)
                    }
                    None => old,
                };
            }
        }
    }

    /// #240: many addresses, each holding one connection that never logs
    /// in and reconnecting as fast as it may. An operator whose page gets a
    /// connection keeps it while the PIN is typed, and for good once logged
    /// in; before, the newest flood connection closed the operator's as the
    /// oldest never let in, within a second.
    #[test]
    fn a_flood_from_many_addresses_cannot_evict_an_operator_before_login() {
        let slots: SharedSlots = Arc::default();
        let start = Instant::now();
        let tick = Duration::from_millis(250);
        let mut flood = Flood::new(96);
        let operator_peer: IpAddr = "10.9.9.9".parse().unwrap();
        let mut operator: Option<(ClientId, Admission, Instant)> = None;
        let mut logged_in = false;
        let mut tries = 0;
        for step in 0..(4 * 240) {
            let at = start + tick * step;
            // Arrivals come in no particular order: the operator's page
            // (retrying each second, as it does) somewhere among them.
            let count = flood.attackers.len();
            let operator_turn = (step as usize * 37) % (count + 1);
            let mut next_attacker = step as usize;
            for k in 0..=count {
                if k == operator_turn {
                    if operator.is_none() && step % 4 == 0 {
                        tries += 1;
                        let admission = Admission::new(at);
                        let id = 1;
                        let arrival = admit(
                            &slots,
                            &mut flood.arrivals,
                            id,
                            Some(operator_peer),
                            at,
                            admission.clone(),
                            None,
                        );
                        if arrival == Arrival::Taken {
                            operator = Some((id, admission, at));
                        }
                    }
                    continue;
                }
                let n = next_attacker % count;
                next_attacker += 1;
                flood.reconnect(&slots, n, at);
            }
            if let Some((_, admission, since)) = &operator {
                assert!(
                    !admission.should_close(at),
                    "operator closed {:?} after connecting",
                    at - *since
                );
                // The PIN is typed within the grace window.
                if !logged_in && at - *since >= LOGIN_GRACE - Duration::from_secs(2) {
                    admission.set(true);
                    logged_in = true;
                }
            }
            assert!(slots.lock().unwrap().open.len() <= MAX_CONNECTIONS);
        }
        let (id, admission, since) = operator.expect("the operator's page never got a slot");
        assert!(logged_in && !evicted(&admission));
        assert!(slots.lock().unwrap().open.iter().any(|s| s.id == id));
        // The flood was real: the pool stayed full and turned arrivals away.
        assert!(flood.taken > 1000, "{}", flood.taken);
        assert!(flood.busy > 0 && flood.too_fast > 0);
        assert!(
            since - start < Duration::from_secs(60),
            "operator in after {tries} tries"
        );
    }

    /// The accept thread, the connections' threads and the command loop
    /// race over the slot table: a connection let in is never closed to
    /// make room, and neither is one inside its grace that is the only one
    /// its address holds.
    #[test]
    fn a_concurrent_flood_never_evicts_a_logged_in_or_fresh_connection() {
        use std::sync::mpsc::channel;

        let slots: SharedSlots = Arc::default();
        let base = Instant::now();
        // Synthetic time: the flood runs a minute after the pool filled.
        let flood_at = base + LOGIN_GRACE * 2;
        let mut stale = Vec::new();
        {
            let mut table = slots.lock().unwrap();
            for n in 0..MAX_CONNECTIONS - 3 {
                stale.extend(fill(
                    &mut table,
                    1 + n as ClientId,
                    1,
                    &address(1000 + n).to_string(),
                    base,
                ));
            }
        }
        let users = {
            let mut table = slots.lock().unwrap();
            let mut users = fill(&mut table, 100, 1, "10.9.9.10", base);
            users.extend(fill(&mut table, 101, 1, "10.9.9.11", base));
            users
        };
        for user in &users {
            user.set(true);
        }
        let operator = {
            let mut table = slots.lock().unwrap();
            let at = flood_at - Duration::from_secs(10);
            fill(&mut table, 102, 1, "10.9.9.9", at).remove(0)
        };
        assert_eq!(slots.lock().unwrap().open.len(), MAX_CONNECTIONS);

        let accepted = Arc::new(AtomicUsize::new(0));
        let (opened, to_close) = channel::<(ClientId, Admission)>();
        let to_close = Arc::new(Mutex::new(to_close));
        // The connections' threads: most close at once (a reconnect), and
        // the command loop lets some in, which then stay.
        let closers: Vec<_> = (0..3)
            .map(|_| {
                let slots = slots.clone();
                let to_close = to_close.clone();
                std::thread::spawn(move || {
                    let mut kept = Vec::new();
                    loop {
                        let next = to_close.lock().unwrap().recv();
                        let Ok((id, admission)) = next else {
                            break;
                        };
                        if id % 7 == 0 {
                            admission.set(true);
                            // Unless it was closed before the log-in came:
                            // its address held another, which grace
                            // protected instead.
                            if admission.state() == ADMITTED {
                                kept.push(admission);
                            }
                        } else {
                            std::thread::yield_now();
                            slots.lock().unwrap().close(id, Instant::now());
                        }
                    }
                    kept
                })
            })
            .collect();
        // The command loop lets the operator in partway through.
        let login = {
            let accepted = accepted.clone();
            let operator = operator.clone();
            std::thread::spawn(move || {
                while accepted.load(Ordering::Acquire) < 200 {
                    std::thread::yield_now();
                }
                operator.set(true);
            })
        };
        // The accept thread.
        let mut arrivals = Arrivals::default();
        let mut outcomes = [0usize; 3];
        for n in 0..4000usize {
            let at = flood_at + Duration::from_millis(n as u64);
            let admission = Admission::new(at);
            let id = 10_000 + n as ClientId;
            let peer = Some(address(n % 256));
            match admit(&slots, &mut arrivals, id, peer, at, admission.clone(), None) {
                Arrival::Taken => {
                    outcomes[0] += 1;
                    opened.send((id, admission)).unwrap();
                }
                Arrival::Busy => outcomes[1] += 1,
                Arrival::TooFast => outcomes[2] += 1,
            }
            accepted.fetch_add(1, Ordering::Release);
            assert!(slots.lock().unwrap().open.len() <= MAX_CONNECTIONS);
        }
        drop(opened);
        login.join().unwrap();
        let kept: Vec<Admission> = closers
            .into_iter()
            .flat_map(|c| c.join().unwrap())
            .collect();

        assert!(!kept.is_empty());
        let end = flood_at + Duration::from_secs(4);
        assert!(!evicted(&operator) && !operator.should_close(end));
        for user in users.iter().chain(&kept) {
            assert!(!evicted(user) && !user.should_close(end));
        }
        let table = slots.lock().unwrap();
        for id in [100, 101, 102] {
            assert!(table.open.iter().any(|s| s.id == id), "{id} lost its slot");
        }
        // The stale ones made room; once the pool held only fresh and
        // logged-in connections, arrivals were turned away.
        assert!(stale.iter().all(evicted));
        assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
    }

    /// Being let in and being evicted race; exactly one wins.
    #[test]
    fn admission_and_eviction_never_both_win() {
        let start = Instant::now();
        let now = start + LOGIN_GRACE;
        for _ in 0..500 {
            let admission = Admission::new(start);
            let other = admission.clone();
            let admit = std::thread::spawn(move || other.set(true));
            let closed = admission.evict(now, false);
            admit.join().unwrap();
            assert_eq!(closed, evicted(&admission));
            assert_eq!(closed, admission.should_close(now));
            assert_eq!(!closed, admission.state() == ADMITTED);
        }
        // Let in, it cannot be evicted; within its grace, neither.
        let admitted = Admission::new(start);
        admitted.set(true);
        assert!(!admitted.evict(now + LOGIN_DEADLINE, false));
        assert!(!admitted.evict(now, true));
        let fresh = Admission::new(start);
        assert!(!fresh.evict(start + LOGIN_GRACE - Duration::from_millis(1), false));
        assert!(fresh.evict(now, false));
        // Evicted is final: a log-in arriving late does not undo it.
        fresh.set(true);
        assert!(fresh.should_close(now));
    }

    /// Each address opens at most a burst, then one every
    /// [`ARRIVAL_REFILL`]; this machine is never limited.
    #[test]
    fn new_connections_are_limited_per_address() {
        let mut arrivals = Arrivals::default();
        let now = Instant::now();
        let peer = Some(address(1));
        for _ in 0..ARRIVAL_BURST as usize {
            assert!(arrivals.allow(peer, now));
        }
        assert!(!arrivals.allow(peer, now));
        // Turned away again: an attempt counts, it does not wait in line.
        assert!(!arrivals.allow(peer, now + ARRIVAL_REFILL / 2));
        // Another address is not held up by it.
        assert!(arrivals.allow(Some(address(2)), now));
        assert!(arrivals.allow(None, now));
        // One more after a refill period, then none.
        let later = now + ARRIVAL_REFILL;
        assert!(arrivals.allow(peer, later));
        assert!(!arrivals.allow(peer, later));
        // Back to a full burst once it has stayed away long enough.
        let rested = later + ARRIVAL_REFILL * (ARRIVAL_BURST as u32);
        for _ in 0..ARRIVAL_BURST as usize {
            assert!(arrivals.allow(peer, rested));
        }
        assert!(!arrivals.allow(peer, rested));
        // Loopback, in either family.
        for local in ["127.0.0.1", "::1"] {
            let local = Some(local.parse().unwrap());
            for _ in 0..1000 {
                assert!(arrivals.allow(local, now));
            }
        }
        // Through admit: an IPv4-mapped address is the same address.
        let slots: SharedSlots = Arc::default();
        let mut arrivals = Arrivals::default();
        let mapped: IpAddr = "::ffff:10.0.0.7".parse().unwrap();
        let plain: IpAddr = "10.0.0.7".parse().unwrap();
        for n in 0..ARRIVAL_BURST as ClientId {
            let peer = if n % 2 == 0 { mapped } else { plain };
            let arrival = admit(
                &slots,
                &mut arrivals,
                n,
                Some(peer),
                now,
                Admission::new(now),
                None,
            );
            // At most MAX_PER_PEER at once from it; past that, busy.
            let expected = if (n as usize) < MAX_PER_PEER {
                Arrival::Taken
            } else {
                Arrival::Busy
            };
            assert_eq!(arrival, expected);
        }
        let arrival = admit(
            &slots,
            &mut arrivals,
            99,
            Some(plain),
            now,
            Admission::new(now),
            None,
        );
        assert_eq!(arrival, Arrival::TooFast);
    }

    /// However many addresses connect, the counts kept stay bounded; an
    /// address being limited is not forgotten to make room for them.
    #[test]
    fn the_arrival_tracker_stays_bounded() {
        let mut arrivals = Arrivals::default();
        let now = Instant::now();
        let limited = Some("10.200.0.1".parse().unwrap());
        while arrivals.allow(limited, now) {}
        for n in 0..20_000usize {
            let peer = IpAddr::from([172, 16 + (n >> 16) as u8, (n >> 8) as u8, n as u8]);
            let at = now + Duration::from_millis(n as u64 / 100);
            assert!(arrivals.allow(Some(peer), at));
            assert!(arrivals.buckets.len() <= MAX_TRACKED_ADDRESSES);
        }
        assert_eq!(arrivals.buckets.len(), MAX_TRACKED_ADDRESSES);
        assert!(arrivals.buckets.contains_key(&limited));
        // Still limited: a fraction of a second has passed since.
        let at = now + Duration::from_millis(20_000 / 100);
        let mut allowed = 0;
        while arrivals.allow(limited, at) {
            allowed += 1;
        }
        assert!(allowed < ARRIVAL_BURST as usize, "{allowed}");
        // Addresses that came and went long ago are forgotten first.
        let rested = at + ARRIVAL_REFILL * (ARRIVAL_BURST as u32 + 1);
        assert!(arrivals.allow(Some(address(7)), rested));
        assert!(arrivals.buckets.len() <= 2 + 1);
    }

    /// A request head sent a byte at a time no longer keeps its slot past
    /// the head deadline.
    #[test]
    fn a_slow_request_head_is_cut_off_at_its_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            for _ in 0..40 {
                if stream.write_all(b"G").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let (mut server_side, _) = listener.accept().unwrap();
        let started = Instant::now();
        let head = read_head(&mut server_side, started + Duration::from_millis(400));
        assert!(head.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(server_side);
        client.join().unwrap();
    }

    /// #266: a few addresses keep the pool full with connections that never
    /// log in, each reopened as soon as its grace is over (half of them
    /// replacing the old one, which closes it to make room; half closing it
    /// first), as fast as they are let. Before, an operator got 503 on
    /// almost every try; now every try gets a slot, and the one that logs
    /// in keeps it.
    #[test]
    fn a_handful_of_addresses_cannot_keep_an_operator_out() {
        let slots: SharedSlots = Arc::default();
        let mut arrivals = Arrivals::default();
        let start = Instant::now();
        let tick = Duration::from_millis(250);
        let peers: Vec<IpAddr> = (0..4u8).map(|n| IpAddr::from([10, 0, 0, 1 + n])).collect();
        // Per address: (id, admission, when it opened it).
        let mut held: Vec<Vec<(ClientId, Admission, Instant)>> = vec![Vec::new(); peers.len()];
        let mut next: ClientId = 10_000;
        let mut reopened = 0usize;
        let operator_peer: IpAddr = "10.9.9.9".parse().unwrap();
        let mut tries = 0usize;
        let mut full_tries = 0usize;
        let mut operator: Option<(ClientId, Admission, Instant)> = None;
        let mut logged_in = false;
        let close = |id: ClientId, at: Instant| slots.lock().unwrap().close(id, at);
        for step in 0..(4 * 180u32) {
            let at = start + tick * step;
            for (p, &peer) in peers.iter().enumerate() {
                let mut open = |arrivals: &mut Arrivals| {
                    let id = next;
                    next += 1;
                    let admission = Admission::new(at);
                    let arrival = admit(
                        &slots,
                        arrivals,
                        id,
                        Some(peer),
                        at,
                        admission.clone(),
                        None,
                    );
                    (arrival == Arrival::Taken).then_some((id, admission, at))
                };
                // Closed to make room, or timed out: gone.
                held[p].retain(|(id, admission, _)| {
                    let ended = admission.should_close(at);
                    if ended {
                        close(*id, at);
                    }
                    !ended
                });
                // Reopen each whose grace is over.
                let mut kept = Vec::new();
                for (id, admission, opened) in std::mem::take(&mut held[p]) {
                    if at - opened < LOGIN_GRACE {
                        kept.push((id, admission, opened));
                        continue;
                    }
                    reopened += 1;
                    if p % 2 == 0 {
                        close(id, at);
                        kept.extend(open(&mut arrivals));
                    } else {
                        match open(&mut arrivals) {
                            Some(new) => {
                                close(id, at);
                                kept.push(new);
                            }
                            None => kept.push((id, admission, opened)),
                        }
                    }
                }
                held[p] = kept;
                // Top up to as many as it may hold.
                while held[p].len() < MAX_PER_PEER {
                    match open(&mut arrivals) {
                        Some(new) => held[p].push(new),
                        None => break,
                    }
                }
            }
            // The operator's page tries every ten seconds until logged in;
            // a try that is not the last is given up at once.
            if !logged_in && step % 40 == 20 {
                if slots.lock().unwrap().open.len() == MAX_CONNECTIONS {
                    full_tries += 1;
                }
                tries += 1;
                let id = next;
                next += 1;
                let admission = Admission::new(at);
                let arrival = admit(
                    &slots,
                    &mut arrivals,
                    id,
                    Some(operator_peer),
                    at,
                    admission.clone(),
                    None,
                );
                assert_eq!(arrival, Arrival::Taken, "try {tries} at {:?}", at - start);
                if tries < 6 {
                    close(id, at);
                } else {
                    operator = Some((id, admission, at));
                }
            }
            if let Some((_, admission, since)) = &operator {
                assert!(!admission.should_close(at), "operator closed");
                // The PIN is typed within a few seconds.
                if !logged_in && at - *since >= Duration::from_secs(5) {
                    admission.set(true);
                    logged_in = true;
                }
            }
            assert!(slots.lock().unwrap().open.len() <= MAX_CONNECTIONS);
        }
        let (id, admission, _) = operator.expect("the operator never got a slot");
        assert!(logged_in && !evicted(&admission));
        assert!(slots.lock().unwrap().open.iter().any(|s| s.id == id));
        // The flood was real: it kept reopening and the pool stayed full.
        assert!(reopened > 200, "{reopened}");
        assert!(
            full_tries >= 4,
            "{full_tries} of {tries} tries met a full pool"
        );
    }

    /// #266: an address that reopens its one connection, by closing it
    /// first or by replacing it, continues its wait instead of starting a
    /// new grace; once that wait is past its grace, a newcomer gets in. One
    /// closed by somebody else gets a new grace when it reconnects.
    #[test]
    fn reopening_a_connection_does_not_buy_a_new_grace() {
        let mut slots = Slots::default();
        let start = Instant::now();
        let mut held: Vec<(IpAddr, ClientId, Admission)> = (0..MAX_CONNECTIONS)
            .map(|n| {
                let admission = Admission::new(start);
                let id = 1 + n as ClientId;
                assert!(slots.open(id, Some(address(n)), start, admission.clone(), None));
                (address(n), id, admission)
            })
            .collect();
        let mut next: ClientId = 1000;
        // Closing first, inside the grace.
        for secs in [15, 29] {
            let at = start + Duration::from_secs(secs);
            for (peer, id, admission) in held.iter_mut() {
                slots.close(*id, at);
                let new = Admission::new(at);
                assert!(slots.open(next, Some(*peer), at, new.clone(), None));
                assert_eq!(new.grace_since(), Some(start));
                (*id, *admission) = (next, new);
                next += 1;
            }
        }
        // Replacing, just past it: its own old one is closed to make room.
        let at = start + LOGIN_GRACE + Duration::from_millis(500);
        for (peer, id, admission) in held.iter_mut().step_by(2) {
            let new = Admission::new(at);
            assert!(slots.open(next, Some(*peer), at, new.clone(), None));
            assert!(evicted(admission));
            slots.close(*id, at);
            assert_eq!(new.grace_since(), Some(start));
            (*id, *admission) = (next, new);
            next += 1;
        }
        assert_eq!(slots.open.len(), MAX_CONNECTIONS);

        // An operator gets in, and is safe in its own grace.
        let at = start + LOGIN_GRACE + Duration::from_secs(1);
        let operator = Admission::new(at);
        let peer = Some("10.9.9.9".parse().unwrap());
        assert!(slots.open(9000, peer, at, operator.clone(), None));
        assert_eq!(operator.grace_since(), Some(at));
        let victim = held.iter().position(|(_, _, a)| evicted(a)).unwrap();
        assert_eq!(held.iter().filter(|(_, _, a)| evicted(a)).count(), 1);
        let peer = Some("10.9.9.10".parse().unwrap());
        assert!(slots.open(9001, peer, at, Admission::new(at), None));
        assert!(!evicted(&operator));

        // The one closed for the operator reconnects with a new grace.
        let (peer, id) = (held[victim].0, held[victim].1);
        slots.close(id, at);
        let back = Admission::new(at);
        let room = Some("10.9.9.11".parse().unwrap());
        assert!(slots.open(9002, room, at, Admission::new(at), None));
        assert!(slots.open(next, Some(peer), at, back.clone(), None));
        assert_eq!(back.grace_since(), Some(at));
    }

    /// #338: an IPv6 address is one address to the per-address limits, so
    /// devices sharing a LAN's `/64` do not share them; the `/64` is what
    /// the limits on top count by (#266). IPv4 and loopback have no `/64`.
    #[test]
    fn ipv6_addresses_count_individually() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let v6 = |s: &str| s.parse::<Ipv6Addr>().unwrap();
        assert_eq!(
            peer_key(ip("2001:db8:1:2:a:b:c:d")),
            ip("2001:db8:1:2:a:b:c:d")
        );
        assert_eq!(peer_key(ip("fe80::1234")), ip("fe80::1234"));
        assert_eq!(peer_key(ip("::ffff:10.0.0.7")), ip("10.0.0.7"));
        assert_eq!(peer_key(ip("10.0.0.7")), ip("10.0.0.7"));
        assert_eq!(peer_key(ip("::1")), ip("::1"));
        assert_eq!(
            prefix_key(ip("2001:db8:1:2:a:b:c:d")),
            Some(v6("2001:db8:1:2::"))
        );
        assert_eq!(prefix_key(ip("fe80::1234")), Some(v6("fe80::")));
        assert_eq!(prefix_key(ip("::ffff:10.0.0.7")), None);
        assert_eq!(prefix_key(ip("10.0.0.7")), None);
        assert_eq!(prefix_key(ip("::1")), None);

        let slots: SharedSlots = Arc::default();
        let mut arrivals = Arrivals::default();
        let now = Instant::now();
        let mut attempt = |n: u32, peer: &str| {
            admit(
                &slots,
                &mut arrivals,
                n as ClientId,
                Some(ip(peer)),
                now,
                Admission::new(now),
                None,
            )
        };
        // Device 1 fills its own per-peer allocation.
        for n in 0..MAX_PER_PEER as u32 {
            assert_eq!(attempt(n, "2001:db8:1:2::1"), Arrival::Taken);
        }
        // Device 1 is now at capacity.
        assert_eq!(
            attempt(MAX_PER_PEER as u32, "2001:db8:1:2::1"),
            Arrival::Busy
        );

        // Device 2 on the same /64 is NOT locked out by Device 1.
        let mut id = MAX_PER_PEER as u32 + 1;
        assert_eq!(attempt(id, "2001:db8:1:2::2"), Arrival::Taken);
        id += 1;

        // Link-local addresses in fe80:: also count individually.
        for _ in 0..MAX_PER_PEER {
            assert_eq!(attempt(id, "fe80::1"), Arrival::Taken);
            id += 1;
        }
        assert_eq!(attempt(id, "fe80::1"), Arrival::Busy);
        id += 1;
        assert_eq!(attempt(id, "fe80::2"), Arrival::Taken);
    }

    /// Address `n` of one IPv6 `/64`, `2001:db8:1:2::/64`.
    fn on_64(n: usize) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(
            0x2001,
            0xdb8,
            1,
            2,
            0,
            0,
            1,
            0x1000 + n as u16,
        ))
    }

    /// #338: one device on an IPv6 LAN opening a connection about once a
    /// second, never logging in, does not keep another device on the same
    /// `/64` out: its connection is taken, and grace protects it while
    /// newcomers from elsewhere fill the pool. Several devices on one `/64`
    /// loading the console at once all get in.
    #[test]
    fn devices_sharing_an_ipv6_64_do_not_lock_each_other_out() {
        let slots: SharedSlots = Arc::default();
        let mut arrivals = Arrivals::default();
        let start = Instant::now();
        let busy = on_64(0);
        let mut next: ClientId = 1;
        let mut held: Vec<(ClientId, Admission)> = Vec::new();
        let mut busy_taken = 0usize;
        for s in 0..120u64 {
            let at = start + Duration::from_secs(s);
            held.retain(|(id, admission)| {
                let ended = admission.should_close(at);
                if ended {
                    slots.lock().unwrap().close(*id, at);
                }
                !ended
            });
            let admission = Admission::new(at);
            let arrival = admit(
                &slots,
                &mut arrivals,
                next,
                Some(busy),
                at,
                admission.clone(),
                None,
            );
            if arrival == Arrival::Taken {
                held.push((next, admission));
                busy_taken += 1;
            }
            next += 1;
        }
        // It really was limited, by its own limits.
        assert_eq!(held.len(), MAX_PER_PEER);
        assert!(busy_taken < 120, "{busy_taken}");

        let at = start + Duration::from_secs(120);
        let second = Admission::new(at);
        let arrival = admit(
            &slots,
            &mut arrivals,
            next,
            Some(on_64(1)),
            at,
            second.clone(),
            None,
        );
        assert_eq!(arrival, Arrival::Taken);
        next += 1;
        // Newcomers from elsewhere close the busy device's connections (its
        // wait is long past its grace), then are turned away: never the
        // second device's.
        let mut newcomers = 0;
        while slots.lock().unwrap().open(
            next,
            Some(address(newcomers)),
            at,
            Admission::new(at),
            None,
        ) {
            next += 1;
            newcomers += 1;
            assert!(newcomers <= MAX_CONNECTIONS);
        }
        assert_eq!(newcomers, MAX_CONNECTIONS - 1);
        assert!(held.iter().all(|(_, a)| evicted(a)));
        assert!(!evicted(&second) && !second.should_close(at));

        // A dozen devices on one /64 loading the page at once (five
        // connections each) all get in.
        let slots: SharedSlots = Arc::default();
        let mut arrivals = Arrivals::default();
        for device in 0..12 {
            for load in 0..5 {
                let arrival = admit(
                    &slots,
                    &mut arrivals,
                    (100 * device + load) as ClientId,
                    Some(on_64(100 + device)),
                    start,
                    Admission::new(start),
                    None,
                );
                assert_eq!(arrival, Arrival::Taken, "device {device}, load {load}");
            }
        }
    }

    /// #266, with #338's per-address limits: one machine taking a new
    /// address in its `/64` for every connection pins at most
    /// [`MAX_PROTECTED_PER_PREFIX`] slots and opens new connections no
    /// faster than its `/64`'s rate, so operators from elsewhere still get
    /// a slot and keep it until they log in.
    #[test]
    fn many_addresses_in_one_ipv6_64_cannot_hold_the_pool() {
        let start = Instant::now();
        // Every slot held, each by its own address of one /64, all inside
        // their grace: newcomers from elsewhere still get in, as all but
        // MAX_PROTECTED_PER_PREFIX of them may be closed.
        let mut slots = Slots::default();
        let mut rotating = Vec::new();
        for n in 0..MAX_CONNECTIONS {
            let peer = on_64(n).to_string();
            rotating.extend(fill(&mut slots, 1 + n as ClientId, 1, &peer, start));
        }
        let at = start + Duration::from_secs(1);
        let mut operators = Vec::new();
        for n in 0..MAX_CONNECTIONS - MAX_PROTECTED_PER_PREFIX {
            let admission = Admission::new(at);
            let id = 1000 + n as ClientId;
            assert!(
                slots.open(id, Some(address(n)), at, admission.clone(), None),
                "newcomer {n}"
            );
            operators.push(admission);
        }
        assert!(!slots.open(2000, Some(address(999)), at, Admission::new(at), None));
        let kept = rotating.iter().filter(|a| !evicted(a)).count();
        assert_eq!(kept, MAX_PROTECTED_PER_PREFIX);
        assert!(operators.iter().all(|a| !evicted(a)));

        // New connections: one burst for the whole /64, whatever the
        // address, then one every PREFIX_ARRIVAL_REFILL. Another /64, and
        // IPv4, are not held up by it.
        let mut arrivals = Arrivals::default();
        let mut n = 0;
        while arrivals.allow(Some(on_64(n)), start) {
            n += 1;
            assert!(n <= 1000);
        }
        assert_eq!(n, PREFIX_ARRIVAL_BURST as usize);
        assert!(!arrivals.allow(Some(on_64(5000)), start));
        assert!(arrivals.allow(Some("2001:db8:1:3::1".parse().unwrap()), start));
        assert!(arrivals.allow(Some(address(1)), start));
        let minute = Duration::from_secs(60);
        let step = Duration::from_millis(100);
        let mut allowed = 0usize;
        for k in 1..=(minute.as_millis() / step.as_millis()) as u32 {
            while arrivals.allow(Some(on_64(n)), start + step * k) {
                n += 1;
                allowed += 1;
            }
        }
        let most = (minute.as_secs_f64() / PREFIX_ARRIVAL_REFILL.as_secs_f64()) as usize;
        assert!(
            allowed <= most && allowed + 2 >= most,
            "{allowed} of {most}"
        );

        // As a flood: the machine keeps the pool full, replacing each of
        // its connections from a new address once its grace is over, and
        // opening more as fast as it is let. Operators come one by one,
        // from an IPv4 address or another /64, and log in five seconds
        // later; each gets a slot at once and keeps it.
        let slots: SharedSlots = Arc::default();
        let mut arrivals = Arrivals::default();
        let tick = Duration::from_millis(250);
        let mut held: Vec<(ClientId, Admission, Instant)> = Vec::new();
        let mut next: ClientId = 10_000;
        let mut next_address = 0usize;
        let mut flood_taken = 0usize;
        let mut operators: Vec<(Admission, Instant)> = Vec::new();
        let mut full_tries = 0usize;
        let steps = 4 * 180u32;
        let close = |id: ClientId, at: Instant| slots.lock().unwrap().close(id, at);
        for step in 0..steps {
            let at = start + tick * step;
            let mut open = |arrivals: &mut Arrivals| {
                let id = next;
                next += 1;
                let peer = on_64(next_address);
                next_address += 1;
                let admission = Admission::new(at);
                let arrival = admit(
                    &slots,
                    arrivals,
                    id,
                    Some(peer),
                    at,
                    admission.clone(),
                    None,
                );
                (arrival == Arrival::Taken).then_some((id, admission, at))
            };
            held.retain(|(id, admission, _)| {
                let ended = admission.should_close(at);
                if ended {
                    close(*id, at);
                }
                !ended
            });
            let mut kept = Vec::new();
            for (id, admission, opened) in std::mem::take(&mut held) {
                if at - opened < LOGIN_GRACE {
                    kept.push((id, admission, opened));
                    continue;
                }
                match open(&mut arrivals) {
                    Some(new) => {
                        flood_taken += 1;
                        close(id, at);
                        kept.push(new);
                    }
                    None => kept.push((id, admission, opened)),
                }
            }
            held = kept;
            while let Some(new) = open(&mut arrivals) {
                flood_taken += 1;
                held.push(new);
            }
            if step % 40 == 20 {
                let k = operators.len();
                let peer = if k % 2 == 0 {
                    IpAddr::from([10, 9, 9, k as u8 + 1])
                } else {
                    IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 9, k as u16, 0, 0, 0, 1))
                };
                if slots.lock().unwrap().open.len() == MAX_CONNECTIONS {
                    full_tries += 1;
                }
                let admission = Admission::new(at);
                let arrival = admit(
                    &slots,
                    &mut arrivals,
                    next,
                    Some(peer),
                    at,
                    admission.clone(),
                    None,
                );
                next += 1;
                assert_eq!(arrival, Arrival::Taken, "operator {k} at {:?}", at - start);
                operators.push((admission, at));
            }
            for (admission, since) in &operators {
                assert!(!admission.should_close(at), "operator closed");
                if at - *since >= Duration::from_secs(5) {
                    admission.set(true);
                }
            }
            assert!(slots.lock().unwrap().open.len() <= MAX_CONNECTIONS);
        }
        assert_eq!(operators.len(), (steps / 40) as usize);
        // The flood was real, and held to its /64's rate.
        let elapsed = tick * steps;
        let most = PREFIX_ARRIVAL_BURST as usize
            + (elapsed.as_secs_f64() / PREFIX_ARRIVAL_REFILL.as_secs_f64()) as usize
            + 1;
        assert!(flood_taken <= most, "{flood_taken} of {most}");
        assert!(flood_taken > 200, "{flood_taken}");
        assert!(
            full_tries + 2 >= operators.len(),
            "{full_tries} of {} tries met a full pool",
            operators.len()
        );
    }

    /// #267: every answer says only this server's own scripts may run, and
    /// where the page may connect; a `Host` that would add to the policy is
    /// left out of it.
    #[test]
    fn pages_may_run_only_this_servers_own_scripts() {
        let csp = content_security_policy(Some("10.0.0.5:8730"));
        for directive in [
            "default-src 'self';",
            "script-src 'self';",
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
            "connect-src 'self' ws://10.0.0.5:8730 wss://10.0.0.5:8730",
        ] {
            assert!(csp.contains(directive), "{directive} missing from {csp}");
        }
        assert!(!csp.contains("unsafe-eval"));
        for host in ["evil.example; script-src *", "a b", "x'y", ""] {
            let csp = content_security_policy(Some(host));
            assert!(csp.ends_with("connect-src 'self'"), "{host}: {csp}");
        }

        // On the wire: once, on the page and on an error alike.
        for head in [
            "GET / HTTP/1.1\r\nHost: 10.0.0.5:8730\r\n",
            "GET /no-such-file HTTP/1.1\r\nHost: 10.0.0.5:8730\r\n",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                serve_asset(&mut stream, &request(head)).unwrap();
            });
            let mut client = TcpStream::connect(address).unwrap();
            let mut raw = Vec::new();
            client.read_to_end(&mut raw).unwrap();
            server.join().unwrap();
            let raw = String::from_utf8_lossy(&raw);
            let (head, _) = raw.split_once("\r\n\r\n").unwrap();
            let policies: Vec<&str> = head
                .lines()
                .filter_map(|line| line.strip_prefix("Content-Security-Policy: "))
                .collect();
            assert_eq!(policies.len(), 1, "{head}");
            assert!(policies[0].contains("script-src 'self';"), "{head}");
            assert!(policies[0].contains("frame-ancestors 'none'"), "{head}");
        }
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The peer `client_peer` makes of a request head from `socket`.
    fn peer_of(socket: &str, head: &str, proxies: &Proxies) -> (Peer, bool) {
        client_peer(Some(ip(socket)), &request(head), &[], proxies)
    }

    fn peer_of_with_names(
        socket: &str,
        head: &str,
        names: &[String],
        proxies: &Proxies,
    ) -> (Peer, bool) {
        client_peer(Some(ip(socket)), &request(head), names, proxies)
    }

    /// #293: behind a reverse proxy on this machine every browser connects
    /// from loopback; only one that names this machine, with no forwarding
    /// header, is taken for a browser here.
    #[test]
    fn a_browser_behind_a_proxy_here_is_not_this_machine() {
        let here = |socket: &str, head: &str| {
            let (peer, through_proxy) = peer_of(socket, head, &Proxies::default());
            assert!(!through_proxy, "{head}");
            assert_eq!(peer.addr, Some(ip(socket)), "{head}");
            peer.local
        };
        for host in [
            "localhost",
            "localhost:8730",
            "127.0.0.1:8730",
            "[::1]:8730",
        ] {
            let head = format!("GET /ws HTTP/1.1\r\nHost: {host}\r\n");
            assert!(here("127.0.0.1", &head), "{host}");
            assert!(here("::ffff:127.0.0.1", &head), "{host}");
            assert!(here("::1", &head), "{host}");
        }
        // The proxy passes on the name the browser used.
        assert!(!here(
            "127.0.0.1",
            "GET /ws HTTP/1.1\r\nHost: mixer.example.org\r\n"
        ));
        // No Host: no browser, and no way to tell.
        assert!(!here("127.0.0.1", "GET /ws HTTP/1.1\r\n"));
        // A proxy that says so, whatever name it passes on.
        for header in [
            "X-Forwarded-For: 203.0.113.7",
            "Forwarded: for=203.0.113.7",
            "X-Real-IP: 203.0.113.7",
            "X-Forwarded-Host: localhost",
            "X-Forwarded-Proto: https",
            "Via: 1.1 proxy",
        ] {
            let head = format!("GET /ws HTTP/1.1\r\nHost: localhost\r\n{header}\r\n");
            assert!(!here("127.0.0.1", &head), "{header}");
        }
        // Another machine naming this one's loopback is still another.
        assert!(!here("10.0.0.7", "GET /ws HTTP/1.1\r\nHost: localhost\r\n"));

        // #351: what counts as being reached from elsewhere.
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let none = Proxies::default();
        assert!(!reached_from_elsewhere(&[], &none));
        assert!(!reached_from_elsewhere(
            &names(&["localhost", "127.0.0.1"]),
            &none
        ));
        assert!(reached_from_elsewhere(
            &names(&["mixer.example.org"]),
            &none
        ));
        let proxy = Proxies {
            addrs: vec![ip("127.0.0.1")],
            header: ForwardedHeader::XForwardedFor,
        };
        assert!(reached_from_elsewhere(&[], &proxy));

        // #351: when a public web host or trusted proxy is configured, no connection
        // (even from loopback naming localhost without forwarding headers) is treated as local.
        let public_names = vec!["mixer.example.org".to_string()];
        let (peer, _) = peer_of_with_names(
            "127.0.0.1",
            "GET /ws HTTP/1.1\r\nHost: localhost\r\n",
            &public_names,
            &Proxies::default(),
        );
        assert!(!peer.local);

        let trusted_proxies = Proxies {
            addrs: vec![ip("10.0.0.1")],
            header: ForwardedHeader::XForwardedFor,
        };
        let (peer, _) = peer_of_with_names(
            "127.0.0.1",
            "GET /ws HTTP/1.1\r\nHost: localhost\r\n",
            &[],
            &trusted_proxies,
        );
        assert!(!peer.local);
    }

    /// #293: a proxy given with `--trusted-proxy` names the client, in the
    /// one header `--forwarded-header` says; nobody else does, and what it
    /// forwards is never this machine.
    #[test]
    fn a_trusted_proxy_names_the_client() {
        use ForwardedHeader::*;
        let proxies = |header| Proxies {
            addrs: vec![ip("127.0.0.1"), ip("10.0.0.1")],
            header,
        };
        let forwarded_for = |socket: &str, header: &str, kind: ForwardedHeader| {
            let head = format!("GET /ws HTTP/1.1\r\nHost: mixer.example.org\r\n{header}\r\n");
            let (peer, through_proxy) = peer_of(socket, &head, &proxies(kind));
            assert!(!peer.local, "{header}");
            through_proxy.then_some(peer.addr).flatten()
        };
        // The last address is the one the proxy added; before it, what the
        // client claimed.
        for (header, kind, client) in [
            ("X-Forwarded-For: 203.0.113.7", XForwardedFor, "203.0.113.7"),
            (
                "X-Forwarded-For: 127.0.0.1, 198.51.100.2",
                XForwardedFor,
                "198.51.100.2",
            ),
            ("X-Forwarded-For: 2001:db8::5", XForwardedFor, "2001:db8::5"),
            (
                "X-Forwarded-For: ::ffff:192.0.2.9",
                XForwardedFor,
                "192.0.2.9",
            ),
            (
                "Forwarded: for=192.0.2.60;proto=http, for=\"[2001:db8::1]:4711\"",
                Forwarded,
                "2001:db8::1",
            ),
            (
                "Forwarded: proto=https;For=\"192.0.2.61:80\"",
                Forwarded,
                "192.0.2.61",
            ),
            ("X-Real-IP: 192.0.2.62", XRealIp, "192.0.2.62"),
        ] {
            assert_eq!(forwarded_for("127.0.0.1", header, kind), Some(ip(client)));
            assert_eq!(
                forwarded_for("::ffff:10.0.0.1", header, kind),
                Some(ip(client))
            );
            // Anyone else's forwarding header is not believed.
            assert_eq!(forwarded_for("10.0.0.2", header, kind), None, "{header}");
            // Nor another header than the one named: the client may have
            // sent it, and a proxy that sets another passes it on.
            for other in [XForwardedFor, Forwarded, XRealIp] {
                if other != kind {
                    assert_eq!(forwarded_for("127.0.0.1", header, other), None, "{header}");
                }
            }
        }
        // A proxy that sets X-Real-IP only, passing on what the client put
        // in X-Forwarded-For: X-Real-IP is the client.
        assert_eq!(
            forwarded_for(
                "127.0.0.1",
                "X-Forwarded-For: 192.0.2.1\r\nX-Real-IP: 203.0.113.9",
                XRealIp
            ),
            Some(ip("203.0.113.9"))
        );
        // A loopback client a proxy names is the proxy's own machine, or a
        // lie: not this machine either way (asserted in `forwarded_for`).
        assert_eq!(
            forwarded_for("127.0.0.1", "X-Forwarded-For: 127.0.0.1", XForwardedFor),
            Some(ip("127.0.0.1"))
        );
        assert_eq!(ForwardedHeader::parse("X-Real-IP"), Some(XRealIp));
        assert_eq!(ForwardedHeader::parse("forwarded"), Some(Forwarded));
        assert_eq!(ForwardedHeader::parse("via"), None);
        // Nothing usable: the proxy itself, not a browser here.
        for header in [
            "X-Forwarded-For: unknown",
            "Forwarded: for=_hidden",
            "X-Other: 1",
        ] {
            let head = format!("GET /ws HTTP/1.1\r\nHost: localhost\r\n{header}\r\n");
            let (peer, through_proxy) = peer_of("127.0.0.1", &head, &proxies(XForwardedFor));
            assert!(!through_proxy && !peer.local, "{header}");
            assert_eq!(peer.addr, Some(ip("127.0.0.1")));
        }
    }

    /// #293: connections a trusted proxy forwards are limited per client,
    /// as if each client had connected directly; one client's do not count
    /// against another's.
    #[test]
    fn clients_behind_a_trusted_proxy_get_their_own_limits() {
        let slots: SharedSlots = Arc::default();
        let arrivals: SharedArrivals = Arc::default();
        let limits = Limits {
            slots: &slots,
            arrivals: &arrivals,
        };
        let now = Instant::now();
        let mut next: ClientId = 1;
        // The proxy's connection, as the accept thread takes it.
        let mut connect = |admission: &Admission| {
            let id = next;
            next += 1;
            let taken = admit(
                &slots,
                &mut arrivals.lock().unwrap(),
                id,
                Some(ip("127.0.0.1")),
                now,
                admission.clone(),
                None,
            );
            assert_eq!(taken, Arrival::Taken);
            id
        };
        let (a, b) = (ip("203.0.113.7"), ip("203.0.113.8"));

        // Never let in: at most MAX_PER_PEER per client.
        for n in 0..MAX_PER_PEER {
            let id = connect(&Admission::new(now));
            assert_eq!(forwarded(limits, id, a, now), Arrival::Taken, "{n}");
        }
        let id = connect(&Admission::new(now));
        assert_eq!(forwarded(limits, id, a, now), Arrival::Busy);
        let id = connect(&Admission::new(now));
        assert_eq!(forwarded(limits, id, b, now), Arrival::Taken);
        {
            let slots = slots.lock().unwrap();
            let held =
                |client: IpAddr| slots.open.iter().filter(|s| s.peer == Some(client)).count();
            assert_eq!((held(a), held(b)), (MAX_PER_PEER, 1));
        }

        // New connections: ARRIVAL_BURST per client, counting all above.
        let c = ip("203.0.113.9");
        for n in 0..ARRIVAL_BURST as usize {
            let admission = Admission::new(now);
            let id = connect(&admission);
            assert_eq!(forwarded(limits, id, c, now), Arrival::Taken, "{n}");
            admission.set(true);
        }
        let id = connect(&Admission::new(now));
        assert_eq!(forwarded(limits, id, c, now), Arrival::TooFast);
        let id = connect(&Admission::new(now));
        assert_eq!(
            forwarded(limits, id, ip("203.0.113.10"), now),
            Arrival::Taken
        );
    }

    /// #293 review: a trusted proxy on another machine speaks for many
    /// clients. Its connections are held to no address's limits on
    /// arrival; once each names its client, that client's limits apply, and
    /// one client's do not count against another's.
    #[test]
    fn clients_behind_a_trusted_proxy_elsewhere_are_limited_apart() {
        let slots: SharedSlots = Arc::default();
        let arrivals: SharedArrivals = Arc::default();
        let limits = Limits {
            slots: &slots,
            arrivals: &arrivals,
        };
        let proxy = ip("10.0.0.1");
        let proxies = Proxies {
            addrs: vec![proxy],
            header: ForwardedHeader::XForwardedFor,
        };
        let now = Instant::now();
        let mut next: ClientId = 1;
        let mut connect = |from: IpAddr| {
            let id = next;
            next += 1;
            let admission = Admission::new(now);
            let arrival = accept(
                limits,
                &proxies,
                id,
                Some(from),
                now,
                admission.clone(),
                None,
            );
            (arrival, id, admission)
        };
        let (a, b) = (ip("203.0.113.7"), ip("203.0.113.8"));

        // Client A, noisy: more connections, never let in, than one address
        // may hold or open in a burst. The proxy is not turned away for it.
        let total = MAX_PER_PEER.max(ARRIVAL_BURST as usize) + 4;
        let mut a_taken = 0;
        for n in 0..total {
            let (arrival, id, _) = connect(proxy);
            assert_eq!(arrival, Arrival::Taken, "{n}");
            if forwarded(limits, id, a, now) == Arrival::Taken {
                a_taken += 1;
            }
        }
        assert_eq!(a_taken, MAX_PER_PEER);
        // Client B behind the same proxy: its own budget, untouched by A's.
        for n in 0..MAX_PER_PEER {
            let (arrival, id, _) = connect(proxy);
            assert_eq!(arrival, Arrival::Taken, "{n}");
            assert_eq!(forwarded(limits, id, b, now), Arrival::Taken, "{n}");
        }
        let (_, id, _) = connect(proxy);
        assert_eq!(forwarded(limits, id, b, now), Arrival::Busy);

        // Client A's arrival budget ran out with its burst; B's is its own.
        let (_, id, admission) = connect(proxy);
        admission.set(true);
        assert_eq!(forwarded(limits, id, a, now), Arrival::TooFast);

        // The same address, not trusted, is one client like any other.
        let other = ip("10.0.0.2");
        let refused = (0..total)
            .map(|_| connect(other).0)
            .filter(|arrival| *arrival != Arrival::Taken)
            .count();
        assert!(refused > 0);
    }

    /// #293 end to end: what the command loop is told about a WebSocket
    /// client, directly and through a proxy here, trusted or not.
    #[test]
    fn the_command_loop_learns_who_a_web_client_is() {
        let joined = |names: Vec<String>, addrs: Vec<IpAddr>, host: &str, extra: &str| -> Peer {
            let (tx, rx) = std::sync::mpsc::channel();
            let bound = serve(
                "127.0.0.1:0".parse().unwrap(),
                tx,
                names,
                Proxies {
                    addrs,
                    header: ForwardedHeader::XForwardedFor,
                },
            )
            .unwrap();
            let mut client = TcpStream::connect(bound).unwrap();
            let head = format!(
                "GET /ws HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n{extra}\r\n"
            );
            client.write_all(head.as_bytes()).unwrap();
            loop {
                match rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(Inbound::Joined(_, _, peer, _)) => return peer,
                    Ok(_) => {}
                    Err(error) => panic!("no client joined: {error}"),
                }
            }
        };
        let lo = Some(ip("127.0.0.1"));
        // A browser here without public web host or trusted proxy.
        assert_eq!(
            joined(Vec::new(), Vec::new(), "localhost:8730", ""),
            Peer {
                addr: lo,
                local: true
            }
        );
        // #351: with a public web host configured, a request naming localhost
        // over loopback is NOT treated as this machine.
        assert_eq!(
            joined(
                vec!["mixer.example.org".to_string()],
                Vec::new(),
                "localhost:8730",
                ""
            ),
            Peer {
                addr: lo,
                local: false
            }
        );
        // Through a proxy here that is not trusted: one shared address,
        // and not this machine.
        let proxied = "X-Forwarded-For: 203.0.113.7\r\n";
        let untrusted = Peer {
            addr: lo,
            local: false,
        };
        assert_eq!(
            joined(
                vec!["mixer.example.org".to_string()],
                Vec::new(),
                "mixer.example.org",
                ""
            ),
            untrusted
        );
        assert_eq!(
            joined(
                vec!["mixer.example.org".to_string()],
                Vec::new(),
                "mixer.example.org",
                proxied
            ),
            untrusted
        );
        // Trusted: the client it forwards for.
        assert_eq!(
            joined(
                vec!["mixer.example.org".to_string()],
                vec![ip("127.0.0.1")],
                "mixer.example.org",
                proxied
            ),
            Peer {
                addr: Some(ip("203.0.113.7")),
                local: false
            }
        );
    }
}
