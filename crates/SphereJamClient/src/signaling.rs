//! The signaling plane: one WebSocket, JSON envelopes, request/response
//! correlation.
//!
//! Deliberately single-threaded. The socket is owned by the jam worker thread
//! and is never shared: reads and writes interleave on one thread, so there is
//! no lock to contend, no half-written frame to reason about, and no way for a
//! caller on another thread to reorder the join sequence. Other threads reach
//! the worker through a command channel, not through this type.
//!
//! Audio never passes through here. This socket carries membership, stream
//! metadata, transport negotiation and clock exchanges; samples go over the
//! media plane, which is a different socket with a different framing.

use std::collections::VecDeque;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde::Serialize;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};
use url::Url;

use crate::error::{JamError, Result};
use crate::protocol::{self, AuthReady, Envelope};

/// How long one blocking read waits before reporting "nothing yet".
const READ_POLL: Duration = Duration::from_millis(200);

/// Ceiling on one inbound signaling message. The largest legitimate message is
/// a room snapshot, which is kilobytes; anything approaching this is a bug or
/// an attempt to make the client allocate.
const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// The blocking signaling client.
pub struct SignalingClient {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    /// Events that arrived while a request was waiting for its reply. They are
    /// held rather than dropped: a stream published during the join handshake
    /// is exactly the one a client cannot afford to miss.
    pending: VecDeque<Envelope>,
    next_request: u64,
    closed: bool,
}

impl SignalingClient {
    /// Open the socket and consume the server's `auth.ready`.
    ///
    /// Authentication happens before the upgrade, so a bad credential is an
    /// HTTP status here rather than a socket that opens and immediately closes.
    pub fn connect(url: &Url, token: &str, timeout: Duration) -> Result<(Self, AuthReady)> {
        crate::crypto::ensure_crypto_provider();
        if token.trim().is_empty() {
            return Err(JamError::Auth(
                "no Futureboard account token was available for the jam".to_string(),
            ));
        }
        // Build the upgrade from the url first, then add our own headers.
        // Handing tungstenite a hand-rolled `Request` makes it validate rather
        // than generate, and the handshake then fails on the
        // `Sec-WebSocket-Key` it expected the caller to have produced.
        let mut request = url.as_str().into_client_request().map_err(|error| {
            JamError::WebSocket(format!("could not build the upgrade: {error}"))
        })?;
        let headers = request.headers_mut();
        // The native carrier for the credential. The subprotocol form exists
        // for browsers, which cannot set a header on an upgrade.
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                JamError::Auth("the account token is not a valid header value".to_string())
            })?,
        );
        headers.insert(
            "sec-websocket-protocol",
            http::HeaderValue::from_static(protocol::SUBPROTOCOL),
        );

        // Bounded at every stage; see `wsconnect`.
        let (socket, response) =
            crate::wsconnect::connect(request, timeout).map_err(upgrade_error)?;
        if response.status().as_u16() != 101 {
            return Err(JamError::WebSocket(format!(
                "the jam service answered the upgrade with {}",
                response.status().as_u16()
            )));
        }

        let mut client = Self {
            socket,
            pending: VecDeque::new(),
            next_request: 1,
            closed: false,
        };
        client.set_read_timeout(Some(READ_POLL))?;

        // `auth.ready` is the first thing the server sends. Waiting for it here
        // means every later call already knows which account this connection
        // acts as, which is what stops a client with two accounts signed in
        // from joining as the wrong one.
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match client.read_envelope()? {
                Some(envelope) if envelope.kind == protocol::message::AUTH_READY => {
                    let ready: AuthReady = envelope.decode()?;
                    if ready.protocol_version != 0 && ready.protocol_version != protocol::VERSION {
                        return Err(JamError::Protocol(format!(
                            "the jam service speaks protocol v{}, this build speaks v{}",
                            ready.protocol_version,
                            protocol::VERSION
                        )));
                    }
                    return Ok((client, ready));
                }
                Some(envelope) if envelope.kind == protocol::message::ERROR => {
                    return Err(JamError::Api(protocol::decode_error(&envelope)));
                }
                Some(other) => client.pending.push_back(other),
                None => continue,
            }
        }
        Err(JamError::WebSocket(
            "the jam service did not identify the connection".to_string(),
        ))
    }

    /// Send a message and wait for the reply the server correlates by request
    /// id.
    ///
    /// Events that arrive first are queued, not discarded. An `error` envelope
    /// carrying this request's id is the failure; one carrying no id is a
    /// connection-level failure and is also returned, because there is nothing
    /// useful to do with a socket the server has just complained about.
    pub fn request<T, P>(
        &mut self,
        kind: &str,
        payload: &P,
        expect: &str,
        timeout: Duration,
    ) -> Result<T>
    where
        T: for<'de> serde::Deserialize<'de>,
        P: Serialize,
    {
        let request_id = self.next_id();
        let envelope = Envelope::new(kind, &request_id, payload)
            .map_err(|error| JamError::Protocol(format!("{kind}: {error}")))?;
        self.write(&envelope)?;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let Some(envelope) = self.read_envelope()? else {
                continue;
            };
            if envelope.kind == protocol::message::ERROR
                && (envelope.request_id == request_id || envelope.request_id.is_empty())
            {
                return Err(JamError::Api(protocol::decode_error(&envelope)));
            }
            if envelope.kind == expect && envelope.request_id == request_id {
                return envelope.decode();
            }
            self.pending.push_back(envelope);
        }
        Err(JamError::WebSocket(format!(
            "the jam service did not answer {kind} within {} ms",
            timeout.as_millis()
        )))
    }

    /// Send a message without waiting for a reply.
    pub fn send<P: Serialize>(&mut self, kind: &str, payload: &P) -> Result<()> {
        let envelope = Envelope::new(kind, "", payload)
            .map_err(|error| JamError::Protocol(format!("{kind}: {error}")))?;
        self.write(&envelope)
    }

    /// Take the next server-initiated event, if one is waiting.
    ///
    /// Returns `None` when the poll interval passed with nothing to report,
    /// which is the normal idle case and not an error.
    pub fn poll(&mut self) -> Result<Option<Envelope>> {
        if let Some(envelope) = self.pending.pop_front() {
            return Ok(Some(envelope));
        }
        self.read_envelope()
    }

    /// Read whatever has arrived, keeping it for [`Self::poll`], so the socket
    /// answers the server's heartbeat while the caller is busy elsewhere.
    ///
    /// Waits at most one read poll.
    pub fn service(&mut self) -> Result<()> {
        if let Some(envelope) = self.read_envelope()? {
            self.pending.push_back(envelope);
        }
        Ok(())
    }

    /// Whether the peer has closed.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Close the socket politely. Errors are ignored: the caller is already
    /// tearing down and a failed close changes nothing.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }

    // ── plumbing ────────────────────────────────────────────────────────────

    fn next_id(&mut self) -> String {
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        format!("req-{id}")
    }

    fn write(&mut self, envelope: &Envelope) -> Result<()> {
        if self.closed {
            return Err(JamError::WebSocket(
                "the signaling socket is closed".to_string(),
            ));
        }
        let text = serde_json::to_string(envelope)
            .map_err(|error| JamError::Protocol(format!("could not encode a message: {error}")))?;
        self.socket
            .send(Message::Text(text.into()))
            .map_err(|error| self.socket_error("send", error))
    }

    /// Read one envelope, or `None` if the poll interval elapsed.
    fn read_envelope(&mut self) -> Result<Option<Envelope>> {
        if self.closed {
            return Err(JamError::WebSocket(
                "the signaling socket is closed".to_string(),
            ));
        }
        // Any pong tungstenite queued in response to a server ping goes out
        // here. Without the flush a healthy but silent client eventually looks
        // dead to the server's heartbeat.
        let _ = self.socket.flush();

        let message = match self.socket.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(error)) if is_timeout(&error) => return Ok(None),
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                self.closed = true;
                return Err(JamError::WebSocket(
                    "the jam service closed the signaling socket".to_string(),
                ));
            }
            Err(error) => return Err(self.socket_error("read", error)),
        };

        match message {
            Message::Text(text) => {
                if text.len() > MAX_MESSAGE_BYTES {
                    return Err(JamError::Protocol(format!(
                        "a signaling message of {} bytes exceeds the {MAX_MESSAGE_BYTES}-byte limit",
                        text.len()
                    )));
                }
                let envelope: Envelope = serde_json::from_str(&text).map_err(|error| {
                    JamError::Protocol(format!("malformed signaling message: {error}"))
                })?;
                if envelope.v != protocol::VERSION {
                    return Err(JamError::Protocol(format!(
                        "signaling message declares protocol v{}, this build speaks v{}",
                        envelope.v,
                        protocol::VERSION
                    )));
                }
                Ok(Some(envelope))
            }
            // The signaling plane is JSON. A binary frame here is either a bug
            // or media that has been pointed at the wrong socket; either way it
            // is not something to guess at.
            Message::Binary(_) => Err(JamError::Protocol(
                "the signaling socket carried a binary frame".to_string(),
            )),
            Message::Close(_) => {
                self.closed = true;
                Err(JamError::WebSocket(
                    "the jam service closed the signaling socket".to_string(),
                ))
            }
            // Ping/Pong/Frame are handled inside tungstenite; nothing to do.
            _ => Ok(None),
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<()> {
        let result = match self.socket.get_ref() {
            MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
            MaybeTlsStream::Rustls(stream) => stream.sock.set_read_timeout(timeout),
            // A stream type this build was not compiled with. Leaving the
            // timeout unset would make `poll` block forever, so say so.
            _ => {
                return Err(JamError::WebSocket(
                    "the signaling socket type does not support a read timeout".to_string(),
                ))
            }
        };
        result.map_err(|error| JamError::WebSocket(format!("read timeout: {error}")))
    }

    fn socket_error(&mut self, what: &str, error: tungstenite::Error) -> JamError {
        if matches!(
            error,
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed
        ) {
            self.closed = true;
        }
        JamError::WebSocket(format!("signaling {what} failed: {error}"))
    }
}

impl Drop for SignalingClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// Turn a failed upgrade into something a user can act on.
///
/// The HTTP status is the useful part: 401 means sign in again, 503 means the
/// service is down. The token is never echoed, and tungstenite's own error text
/// never contains it.
fn upgrade_error(error: tungstenite::Error) -> JamError {
    if let tungstenite::Error::Http(response) = &error {
        let status = response.status();
        let body = response
            .body()
            .as_ref()
            .and_then(|bytes| serde_json::from_slice::<crate::error::WireError>(bytes).ok());
        if let Some(wire) = body {
            return JamError::Api(wire);
        }
        return JamError::Api(crate::error::WireError {
            code: match status.as_u16() {
                401 => crate::error::ErrorCode::Unauthenticated,
                403 => crate::error::ErrorCode::Forbidden,
                503 => crate::error::ErrorCode::Unavailable,
                _ => crate::error::ErrorCode::Internal,
            },
            message: format!("the jam service refused the signaling upgrade ({status})"),
            retryable: status.is_server_error(),
            request_id: String::new(),
        });
    }
    JamError::WebSocket(format!("could not open the signaling socket: {error}"))
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{message, JamJoinRequest};

    #[test]
    fn an_outbound_envelope_carries_the_version_and_the_request_id() {
        let envelope = Envelope::new(
            message::JAM_JOIN,
            "req-1",
            &JamJoinRequest {
                jam_id: "jam_1".to_string(),
                device_id: "studio-mac".to_string(),
                ..Default::default()
            },
        )
        .expect("encodes");
        let json = serde_json::to_string(&envelope).expect("serializes");
        assert!(json.contains("\"v\":1"));
        assert!(json.contains("\"type\":\"jam.join\""));
        assert!(json.contains("\"request_id\":\"req-1\""));
        // Optional fields the server defaults are left out entirely.
        assert!(!json.contains("resume_token"));
        assert!(!json.contains("last_seq"));
    }

    #[test]
    fn an_envelope_with_no_request_id_omits_the_field() {
        let envelope =
            Envelope::new(message::PING, "", &crate::protocol::Ping::default()).expect("encodes");
        let json = serde_json::to_string(&envelope).expect("serializes");
        assert!(!json.contains("request_id"));
    }

    #[test]
    fn a_missing_credential_fails_before_any_socket_is_opened() {
        let url = Url::parse("ws://127.0.0.1:1/v1/realtime").expect("url");
        match SignalingClient::connect(&url, "   ", Duration::from_millis(1)) {
            Err(error) => assert!(matches!(error, JamError::Auth(_))),
            Ok(_) => panic!("an empty credential must be refused before dialling"),
        }
    }
}
