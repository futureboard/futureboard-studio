//! Futureboard Audio Jam — the native client.
//!
//! This crate talks to the Futureboard Jam Server: REST for the control plane,
//! one WebSocket for signaling, and a binary media plane over UDP, TCP or TLS.
//! It is the same protocol the browser listener speaks, translated from the
//! server's own `pkg/protocol`, so a Studio and a web guest are two clients of
//! one room rather than two nearly-compatible implementations.
//!
//! # What this crate is not
//!
//! It owns **no audio device, no mixer, no resampler and no UI**. Futureboard
//! Studio already has all four, and a jam that opened its own capture stream
//! would fight the DAW for the hardware. Instead the host implements two small
//! traits — [`bridge::JamAudioSink`] for arriving audio and
//! [`bridge::JamPublishSource`] for outgoing audio — and the engine keeps
//! owning the device:
//!
//! ```text
//! hardware ─▶ Futureboard Audio Engine ─▶ JamPublishSource ─▶ jam
//! jam ─▶ JamAudioSink ─▶ Futureboard Audio Engine ─▶ track
//! ```
//!
//! # Ingress and egress
//!
//! Both directions are explicit, and they are separate decisions.
//!
//! **Egress** is [`JamSession::publish`]: the host says what it sends and from
//! where. Nothing leaves this client that was not published.
//!
//! **Ingress** is [`JamSession::subscribe`]. By default — [`JamIngress::Everything`]
//! — the server sends this client every stream it can decode, which is what a
//! listener wants. A DAW wants [`JamIngress::Routed`]: the session goes silent
//! the moment it joins, and each stream a project routes to a track is asked
//! for by name. At roughly 1.5 Mbit/s per 48 kHz stereo PCM stream, that is the
//! difference between two routed tracks and a whole band arriving on the one
//! link that is already the constraint.
//!
//! A stream that is asked for before it is published is remembered rather than
//! refused: a track bound to a performer who has not started is a track
//! waiting, and it attaches by itself when they publish.
//!
//! # Threads
//!
//! Three, none of them the audio thread:
//!
//! * `jam-control` owns the signaling socket, the room registry and the
//!   reconnect policy.
//! * `jam-media-rx` reads packets, reorders them, decodes them and calls the
//!   sink.
//! * `jam-media-tx` pulls from publish sources and puts packets on the wire.
//!
//! The sink is expected to copy into a lock-free ring the audio callback
//! drains, and a publish source to drain a ring the audio callback fills. No
//! call in this crate is ever made from a realtime context.
//!
//! # Identity
//!
//! `user_id` is immutable and is the only value anything routes on. A username
//! is a display alias its owner may change and someone else may later take, so
//! nothing here keys a map on one. One account may hold several participants in
//! the same jam — a laptop and a phone are two devices — so participants are
//! keyed by participant id and merely *grouped* by account for display.
//!
//! # Getting started
//!
//! ```no_run
//! use std::sync::Arc;
//! use sphere_jam_client::{
//!     bridge::NullSink,
//!     config::JamConfig,
//!     credentials::{CredentialFn, SharedCredentials},
//!     ids::JamId,
//!     session::{JamSession, JamSessionOptions},
//! };
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let config = JamConfig::from_env()?;
//! let credentials: SharedCredentials =
//!     Arc::new(CredentialFn(|| Ok("the host's account token".to_string())));
//! let options = JamSessionOptions::new("studio-mac", Arc::new(NullSink));
//!
//! let session = JamSession::spawn(config, credentials, options)?;
//! session.join(JamId::new("jam_01K4S8QP0M9TZ3B7VNXW1D5CJH"), "")?;
//! # Ok(())
//! # }
//! ```

pub mod api;
pub mod bridge;
pub mod clock;
pub mod config;
pub mod credentials;
pub mod crypto;
pub mod error;
pub mod ids;
pub mod jitter;
pub mod media;
pub mod packet;
pub mod protocol;
pub mod registry;
pub mod session;
pub mod signaling;
pub mod transport;
mod wsconnect;

pub use bridge::{
    ChannelMapping, JamAudioFrame, JamAudioSink, JamPublishRequest, JamPublishSource,
    JamPublishSourceKind, JamTrackBinding, PulledBlock,
};
pub use config::{JamConfig, JamEnv, RegionPreference};
pub use credentials::{JamCredentialProvider, SharedCredentials};
pub use error::{ErrorCode, JamError, Result};
pub use ids::{DeviceId, JamId, MediaAlias, ParticipantId, StreamId, UserId};
pub use protocol::{SubscriptionMode, SubscriptionRefusal};
pub use registry::{JamRegistry, Participant, RemoteStream};
pub use session::{
    JamCommand, JamEvent, JamIngress, JamSession, JamSessionOptions, JamSnapshot, JamState,
};

/// A device id that is stable for this installation.
///
/// The server treats (account, device) as one participant, so reusing this
/// across restarts is what makes a relaunch re-attach rather than leave a ghost
/// participant in the room until its resume window expires. The host is
/// expected to persist the returned value; this only mints one.
pub fn generate_device_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let salt: u64 = rand::random();
    format!("{prefix}-{nanos:016x}{salt:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_device_id_is_prefixed_and_unique() {
        let first = generate_device_id("studio");
        let second = generate_device_id("studio");
        assert!(first.starts_with("studio-"));
        assert_ne!(first, second);
    }
}
