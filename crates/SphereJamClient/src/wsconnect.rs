//! Opening a WebSocket with a deadline.
//!
//! `tungstenite::connect` resolves, connects and runs the TLS and HTTP upgrade
//! handshakes on a blocking socket with no timeout at all. A server that
//! accepts the TCP connection and then says nothing — a half-dead node, a
//! captive portal, a proxy holding the request — left the jam worker blocked
//! there indefinitely: the session sat in Connecting, and Leave waited behind
//! it. Here the TCP connect is bounded and both handshakes run with read and
//! write timeouts, so every stage gives up within the caller's budget. The
//! caller sets its own poll timeout once the socket is open.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use tungstenite::client::IntoClientRequest;
use tungstenite::error::UrlError;
use tungstenite::handshake::client::Response;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{HandshakeError, WebSocket};

/// The socket type every caller in this crate already uses.
pub(crate) type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// [`tungstenite::connect`], with `timeout` applied to the TCP connect (per
/// resolved address) and to every read and write of the handshakes.
///
/// Name resolution is the one stage still left to the system resolver.
pub(crate) fn connect(
    request: impl IntoClientRequest,
    timeout: Duration,
) -> Result<(Socket, Response), tungstenite::Error> {
    // rustls needs a process-wide provider before the first TLS handshake.
    crate::crypto::ensure_crypto_provider();
    let request = request.into_client_request()?;
    let uri = request.uri();
    let host = uri
        .host()
        .ok_or(tungstenite::Error::Url(UrlError::NoHostName))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("wss") | Some("https") => 443,
        _ => 80,
    });
    let timeout = timeout.max(Duration::from_millis(100));

    let mut last_error = None;
    let mut stream = None;
    for address in (host.as_str(), port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let stream = stream.ok_or_else(|| {
        tungstenite::Error::Io(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{host} did not resolve to any address"),
            )
        }))
    })?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let _ = stream.set_nodelay(true);

    match tungstenite::client_tls_with_config(request, stream, None, None) {
        Ok(opened) => Ok(opened),
        Err(HandshakeError::Failure(error)) => Err(error),
        // A blocking socket only interrupts a handshake when a read or write
        // ran out of time.
        Err(HandshakeError::Interrupted(_)) => Err(tungstenite::Error::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "the server did not finish the handshake in time",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;

    /// A server that accepts and then says nothing must not hold the caller
    /// past its budget — the hang this module exists to end.
    #[test]
    fn a_silent_server_times_out_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let holder = std::thread::spawn(move || {
            // Accept and keep the connection open, silently.
            let accepted = listener.accept();
            std::thread::sleep(Duration::from_millis(1500));
            drop(accepted);
        });

        let started = Instant::now();
        let result = connect(
            format!("ws://127.0.0.1:{port}/v1/realtime"),
            Duration::from_millis(300),
        );
        assert!(
            result.is_err(),
            "a silent server cannot complete an upgrade"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "gave up after {:?}, not within the budget",
            started.elapsed()
        );
        let _ = holder.join();
    }

    #[test]
    fn a_closed_port_fails_fast() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let result = connect(format!("ws://127.0.0.1:{port}/"), Duration::from_secs(2));
        assert!(result.is_err());
    }
}
