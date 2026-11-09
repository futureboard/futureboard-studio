//! The media runtime: two threads around one open transport.
//!
//! ```text
//!   network ──▶ receive thread ──▶ parse ──▶ jitter buffer ──▶ decode ──▶ sink
//!                                                                         │
//!                                                            (host ring)  ▼
//!                                                                  audio thread
//!
//!   audio thread ──▶ (host ring) ──▶ publish thread ──▶ packetise ──▶ network
//! ```
//!
//! Neither thread is the audio thread and neither ever becomes one. The sink is
//! expected to copy into a lock-free ring the audio callback drains, and a
//! publish source is expected to drain a ring the audio callback fills. That
//! boundary is the whole reason this file exists: if the audio callback ever
//! waited on a socket, a jam would cost a dropout every time the network
//! hiccuped.
//!
//! The runtime is disposable. A transport change tears it down and builds a new
//! one; the participant, its streams and its subscriptions live in the control
//! plane and are untouched.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::bridge::{JamAudioFrame, JamAudioSink, JamPublishSource};
use crate::error::{JamError, Result};
use crate::ids::StreamId;
use crate::jitter::{JitterBuffer, JitterStats};
use crate::packet::{self, AudioPacketHeader, ControlType, MediaErrorPayload};
use crate::protocol::{AudioFormat, LatencyMetadata, SampleFormat, TransportKind};
use crate::transport::{MediaTransport, RecvOutcome, TransportSender, TransportStatsSnapshot};

/// How long the publish thread idles when no source produced a block. Short
/// enough that a 2.7 ms packet is never held up by the sleep itself.
const PUBLISH_IDLE: Duration = Duration::from_millis(1);

/// One stream this client is receiving, as the media thread needs it.
#[derive(Debug, Clone)]
pub struct SubscribedStream {
    pub id: StreamId,
    pub format: AudioFormat,
    pub latency: LatencyMetadata,
}

/// One stream this client is publishing.
pub struct Publication {
    pub stream_id: StreamId,
    pub alias: u32,
    pub format: AudioFormat,
    pub source: Arc<dyn JamPublishSource>,
    /// Per stream and per generation, incrementing by one per packet.
    sequence: u64,
    /// Whether the next packet is the first of a take.
    take_start: bool,
}

impl Publication {
    pub fn new(
        stream_id: StreamId,
        alias: u32,
        format: AudioFormat,
        source: Arc<dyn JamPublishSource>,
    ) -> Self {
        Self {
            stream_id,
            alias,
            format,
            source,
            sequence: 0,
            take_start: true,
        }
    }
}

/// Live counters for one stream, for the panel.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamRuntimeStats {
    pub jitter: JitterStats,
    /// Peak sample magnitude since the last read, in linear scale.
    pub peak: f32,
}

/// Everything the media threads share with the control thread.
///
/// A `RwLock` rather than a channel because the receive path reads the stream
/// table once per packet and the control thread writes it a handful of times
/// per session: readers must never queue behind each other, and a publish is
/// rare enough that the write lock is invisible.
pub struct MediaShared {
    pub streams: RwLock<HashMap<u32, SubscribedStream>>,
    pub publications: Mutex<Vec<Publication>>,
    pub stats: RwLock<HashMap<StreamId, StreamRuntimeStats>>,
    /// The publisher jam alias every outbound header carries.
    pub jam_alias: AtomicU64,
    /// The connection generation the media node accepted.
    pub generation: std::sync::atomic::AtomicU32,
    /// Set when a media thread notices the transport is gone, so the control
    /// thread can reconnect instead of waiting for a signaling timeout.
    pub transport_lost: AtomicBool,
}

impl MediaShared {
    pub fn new() -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
            publications: Mutex::new(Vec::new()),
            stats: RwLock::new(HashMap::new()),
            jam_alias: AtomicU64::new(0),
            generation: std::sync::atomic::AtomicU32::new(0),
            transport_lost: AtomicBool::new(false),
        }
    }

    /// Replace the receiving stream table.
    pub fn set_streams(&self, streams: HashMap<u32, SubscribedStream>) {
        if let Ok(mut guard) = self.streams.write() {
            *guard = streams;
        }
    }

    pub fn stats_for(&self, stream: &StreamId) -> Option<StreamRuntimeStats> {
        self.stats.read().ok()?.get(stream).copied()
    }

    pub fn all_stats(&self) -> HashMap<StreamId, StreamRuntimeStats> {
        self.stats
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

impl Default for MediaShared {
    fn default() -> Self {
        Self::new()
    }
}

/// An open media transport with its two threads running.
pub struct MediaRuntime {
    pub kind: TransportKind,
    pub candidate_id: String,
    pub node_id: String,
    pub handshake_rtt: Duration,
    shared: Arc<MediaShared>,
    sender: Arc<dyn TransportSender>,
    stop: Arc<AtomicBool>,
    receive: Option<JoinHandle<()>>,
    publish: Option<JoinHandle<()>>,
    stats: Arc<crate::transport::TransportStats>,
}

impl MediaRuntime {
    /// Take ownership of an open transport and start pumping it.
    pub fn start(
        transport: MediaTransport,
        shared: Arc<MediaShared>,
        sink: Arc<dyn JamAudioSink>,
    ) -> Self {
        let MediaTransport {
            kind,
            candidate_id,
            welcome,
            handshake_rtt,
            sender,
            receiver,
            stats,
        } = transport;

        shared.jam_alias.store(welcome.jam_alias, Ordering::Release);
        shared
            .generation
            .store(welcome.generation, Ordering::Release);
        shared.transport_lost.store(false, Ordering::Release);

        let stop = Arc::new(AtomicBool::new(false));
        let keepalive = if welcome.keepalive_seconds > 0 {
            Duration::from_secs(welcome.keepalive_seconds as u64)
        } else {
            Duration::from_secs(15)
        };
        let max_payload = if welcome.max_payload_bytes > 0 {
            welcome.max_payload_bytes as usize
        } else {
            1200
        };

        let receive = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            let sender = Arc::clone(&sender);
            std::thread::Builder::new()
                .name("jam-media-rx".to_string())
                .spawn(move || {
                    receive_loop(receiver, sender, shared, sink, stop, keepalive);
                })
                .ok()
        };
        let publish = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            let sender = Arc::clone(&sender);
            std::thread::Builder::new()
                .name("jam-media-tx".to_string())
                .spawn(move || {
                    publish_loop(sender, shared, stop, max_payload);
                })
                .ok()
        };

        Self {
            kind,
            candidate_id,
            node_id: welcome.node_id,
            handshake_rtt,
            shared,
            sender,
            stop,
            receive,
            publish,
            stats,
        }
    }

    pub fn transport_stats(&self) -> TransportStatsSnapshot {
        self.stats.snapshot()
    }

    /// Whether a media thread has reported the transport gone.
    pub fn lost(&self) -> bool {
        self.shared.transport_lost.load(Ordering::Acquire)
    }

    pub fn sender(&self) -> Arc<dyn TransportSender> {
        Arc::clone(&self.sender)
    }

    /// Say goodbye and stop both threads.
    pub fn shutdown(&mut self) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(frame) = packet::encode_control(
            ControlType::Bye,
            Some(&MediaErrorPayload {
                code: "leaving".to_string(),
                message: "the client is leaving".to_string(),
            }),
        ) {
            let _ = self.sender.send_frame(&frame);
        }
        self.sender.close();
        if let Some(handle) = self.receive.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.publish.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MediaRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Reads frames, reorders them, decodes them, and hands them to the sink.
fn receive_loop(
    mut receiver: Box<dyn crate::transport::TransportReceiver>,
    sender: Arc<dyn TransportSender>,
    shared: Arc<MediaShared>,
    sink: Arc<dyn JamAudioSink>,
    stop: Arc<AtomicBool>,
    keepalive: Duration,
) {
    // Everything the loop needs is allocated once. A jam that has been running
    // for an hour does the same work per packet as one that just started.
    let mut frame = Vec::with_capacity(crate::packet::MAX_MEDIA_FRAME);
    let mut decoded: Vec<f32> = Vec::with_capacity(4096);
    let mut buffers: HashMap<u32, JitterBuffer> = HashMap::new();
    let mut last_keepalive = Instant::now();
    let mut last_heard = Instant::now();
    let silence_limit = silence_limit(keepalive);
    let ping = packet::encode_control::<()>(ControlType::Ping, None).unwrap_or_default();

    while !stop.load(Ordering::Acquire) {
        match receiver.recv_frame(&mut frame) {
            Ok(RecvOutcome::Frame) => {
                last_heard = Instant::now();
                handle_frame(&frame, &shared, &sink, &mut buffers, &mut decoded, &sender);
            }
            Ok(RecvOutcome::Timeout) => {
                // A datagram socket never reports the far end gone. After the
                // node reaps this session, or a NAT rebinding or a Wi-Fi roam
                // changes this client's address, the node drops every packet
                // silently and the jam would read Connected with no audio for
                // good. The node answers every ping with a pong, so a live
                // link always hears something within one keepalive; hearing
                // nothing for several means the link is dead, and the session
                // reconnects instead of waiting on it.
                if last_heard.elapsed() >= silence_limit {
                    shared.transport_lost.store(true, Ordering::Release);
                    break;
                }
            }
            Ok(RecvOutcome::Closed) => {
                shared.transport_lost.store(true, Ordering::Release);
                break;
            }
            Err(_) => {
                // A malformed frame is not a reason to drop the session; a dead
                // socket is. `recv_frame` only errors on the latter.
                shared.transport_lost.store(true, Ordering::Release);
                break;
            }
        }

        // NAT bindings for UDP commonly expire around thirty seconds, so an
        // idle receiver still has to speak.
        if !ping.is_empty() && last_keepalive.elapsed() >= keepalive {
            last_keepalive = Instant::now();
            if sender.send_frame(&ping).is_err() {
                shared.transport_lost.store(true, Ordering::Release);
                break;
            }
        }
    }
}

/// How long the media link may stay silent before it counts as lost: two and a
/// half keepalives, so one lost pong is tolerated and two are not.
fn silence_limit(keepalive: Duration) -> Duration {
    keepalive.saturating_mul(5) / 2
}

fn handle_frame(
    frame: &[u8],
    shared: &MediaShared,
    sink: &Arc<dyn JamAudioSink>,
    buffers: &mut HashMap<u32, JitterBuffer>,
    decoded: &mut Vec<f32>,
    sender: &Arc<dyn TransportSender>,
) {
    let control = match packet::frame_is_control(frame) {
        Ok(value) => value,
        // A frame this build cannot classify is dropped. It is never a reason
        // to panic and never a reason to close a working session.
        Err(_) => return,
    };

    if control {
        let Ok((kind, payload)) = packet::decode_control(frame) else {
            return;
        };
        match kind {
            ControlType::Ping => {
                if let Ok(pong) = packet::encode_control(ControlType::Pong, Some(&payload.to_vec()))
                {
                    let _ = sender.send_frame(&pong);
                }
            }
            ControlType::Bye | ControlType::Error => {
                shared.transport_lost.store(true, Ordering::Release);
            }
            _ => {}
        }
        return;
    }

    let Ok((header, payload)) = packet::decode_audio(frame) else {
        return;
    };
    // A packet stamped with another jam is either a routing bug or an attempt
    // to inject audio into this room. Neither is worth decoding.
    if header.jam_alias != shared.jam_alias.load(Ordering::Acquire) {
        return;
    }

    let Some(stream) = shared
        .streams
        .read()
        .ok()
        .and_then(|guard| guard.get(&header.stream_alias).cloned())
    else {
        // Audio for a stream this client has not been told about yet. It
        // arrives routinely in the window between a publish and its
        // `audio.format_selected`.
        return;
    };

    let buffer = buffers.entry(header.stream_alias).or_default();
    if !buffer.push(header, payload) {
        record_stats(shared, &stream.id, buffer.stats(), None);
        return;
    }

    while let Some(queued) = buffer.pop() {
        deliver(
            &stream,
            &queued.header,
            &queued.payload,
            sink,
            decoded,
            shared,
            buffer.target_packets(),
        );
        buffer.recycle(queued.payload);
    }
    let stats = buffer.stats();
    record_stats(shared, &stream.id, stats, None);
}

fn deliver(
    stream: &SubscribedStream,
    header: &AudioPacketHeader,
    payload: &[u8],
    sink: &Arc<dyn JamAudioSink>,
    decoded: &mut Vec<f32>,
    shared: &MediaShared,
    target_packets: usize,
) {
    let channels = if header.channels > 0 {
        header.channels as usize
    } else {
        stream.format.channels.max(1) as usize
    };

    match stream.format.codec {
        crate::protocol::AudioCodec::Pcm => {
            let format = if stream.format.format == SampleFormat::None {
                SampleFormat::F32Le
            } else {
                stream.format.format
            };
            if header.has(packet::FLAG_SILENCE) {
                // A transmit gap: the payload is empty and the receiver must
                // insert silence rather than conceal a loss it did not have.
                // The header's counts are untrusted: a gap must agree with the
                // negotiated channel count and may never describe more audio
                // than a real packet of this format could carry.
                if stream.format.channels > 0 && channels != stream.format.channels as usize {
                    return;
                }
                let Some(samples) = silence_samples(header.frames, channels, format) else {
                    return;
                };
                decoded.clear();
                decoded.resize(samples, 0.0);
            } else {
                packet::pcm_to_f32(payload, format, channels, decoded);
            }
        }
        // AAC-LC is negotiated by the protocol and is not decoded by this
        // build. Dropping the packet is honest; synthesising silence would look
        // like a working stream that happens to be quiet.
        crate::protocol::AudioCodec::AacLc => return,
    }

    if decoded.is_empty() {
        return;
    }
    let frames = decoded.len() / channels.max(1);

    // The presentation instant is the capture instant plus what this receiver
    // is holding back. It is a monitoring number and never reaches a recording.
    let held = (target_packets as u64).saturating_mul(frames as u64);

    let mut peak = 0.0f32;
    for sample in decoded.iter() {
        let magnitude = sample.abs();
        if magnitude > peak {
            peak = magnitude;
        }
    }

    sink.deliver(
        &stream.id,
        JamAudioFrame {
            capture_timestamp: header.capture_timestamp,
            presentation_timestamp: header.capture_timestamp.saturating_add(held),
            sequence: header.sequence,
            frames,
            channels,
            sample_rate: stream.format.sample_rate.max(0) as u32,
            flags: header.flags,
            samples: decoded,
            latency: stream.latency,
        },
    );

    if header.has(packet::FLAG_END_OF_STREAM) {
        sink.stream_ended(&stream.id);
    }
    record_stats(shared, &stream.id, JitterStats::default(), Some(peak));
}

/// Interleaved sample count a silence packet may expand to, or `None` when its
/// header describes more audio than one media packet of `format` could carry.
///
/// A silence packet has no payload, so nothing but this check stops a header
/// claiming 65 535 frames of 255 channels from making every listener allocate
/// and process ~16.7 million samples on the receive thread.
fn silence_samples(frames: u16, channels: usize, format: SampleFormat) -> Option<usize> {
    let bytes_per_sample = format.bytes_per_sample();
    if channels == 0 || bytes_per_sample == 0 {
        return None;
    }
    let samples = (frames as usize).checked_mul(channels)?;
    let max_samples = (packet::MAX_MEDIA_FRAME - packet::HEADER_SIZE) / bytes_per_sample;
    (samples <= max_samples).then_some(samples)
}

/// Merge live counters for one stream.
///
/// Peaks are held rather than replaced so a UI polling at 30 Hz sees the loudest
/// sample between reads instead of whatever happened to be in the last packet.
fn record_stats(shared: &MediaShared, stream: &StreamId, jitter: JitterStats, peak: Option<f32>) {
    let Ok(mut guard) = shared.stats.write() else {
        return;
    };
    let entry = guard.entry(stream.clone()).or_default();
    if jitter != JitterStats::default() {
        entry.jitter = jitter;
    }
    if let Some(peak) = peak {
        entry.peak = entry.peak.max(peak);
    }
}

/// Pulls from every publish source and puts packets on the wire.
fn publish_loop(
    sender: Arc<dyn TransportSender>,
    shared: Arc<MediaShared>,
    stop: Arc<AtomicBool>,
    max_payload: usize,
) {
    let mut samples: Vec<f32> = Vec::with_capacity(4096);
    let mut payload: Vec<u8> = Vec::with_capacity(max_payload);
    let mut frame: Vec<u8> = Vec::with_capacity(packet::HEADER_SIZE + max_payload);

    while !stop.load(Ordering::Acquire) {
        let jam_alias = shared.jam_alias.load(Ordering::Acquire);
        let generation = shared.generation.load(Ordering::Acquire);
        let mut produced = false;

        let Ok(mut publications) = shared.publications.lock() else {
            break;
        };
        for publication in publications.iter_mut() {
            let format = publication.format;
            let channels = format.channels.max(1) as usize;
            let sample_format = if format.format == SampleFormat::None {
                SampleFormat::F32Le
            } else {
                format.format
            };
            // Never offer more frames than one packet can carry: the media node
            // refuses an oversized payload, and a fragmented datagram is worse
            // than a smaller one.
            let per_frame_bytes = channels * sample_format.bytes_per_sample();
            let capacity_frames = max_payload.checked_div(per_frame_bytes).unwrap_or(0);
            let wanted = (format.frame_samples.max(1) as usize).min(capacity_frames.max(1));

            let Some(block) = publication.source.pull(&mut samples, wanted) else {
                continue;
            };
            if block.frames == 0 || samples.is_empty() {
                continue;
            }
            produced = true;

            packet::f32_to_pcm(&samples, sample_format, &mut payload);
            let header = AudioPacketHeader {
                version: packet::PACKET_VERSION,
                codec: crate::protocol::AudioCodec::Pcm,
                flags: if publication.take_start || block.take_start {
                    packet::FLAG_MARKER
                } else {
                    0
                },
                jam_alias,
                stream_alias: publication.alias,
                generation,
                sequence: publication.sequence,
                capture_timestamp: block.capture_ticks,
                frames: block.frames.min(u16::MAX as usize) as u16,
                channels: channels.min(u8::MAX as usize) as u8,
            };
            publication.take_start = false;
            publication.sequence = publication.sequence.wrapping_add(1);

            frame.clear();
            frame.resize(packet::HEADER_SIZE + payload.len(), 0);
            if header.encode(&mut frame).is_err() {
                continue;
            }
            frame[packet::HEADER_SIZE..].copy_from_slice(&payload);
            if sender.send_frame(&frame).is_err() {
                shared.transport_lost.store(true, Ordering::Release);
                return;
            }
        }
        drop(publications);

        if !produced {
            std::thread::sleep(PUBLISH_IDLE);
        }
    }
}

/// Build the media header for one outbound packet. Exposed for tests and for a
/// host that packetises on its own thread.
///
/// The parameter list is the header's own field list; grouping it into a struct
/// would only move the same values one indirection away.
#[allow(clippy::too_many_arguments)]
pub fn outbound_header(
    jam_alias: u64,
    stream_alias: u32,
    generation: u32,
    sequence: u64,
    capture_ticks: u64,
    frames: usize,
    channels: usize,
    take_start: bool,
) -> Result<AudioPacketHeader> {
    if frames > u16::MAX as usize {
        return Err(JamError::Audio(format!(
            "a media packet cannot carry {frames} frames"
        )));
    }
    if channels == 0 || channels > u8::MAX as usize {
        return Err(JamError::Audio(format!(
            "a media packet cannot carry {channels} channels"
        )));
    }
    Ok(AudioPacketHeader {
        version: packet::PACKET_VERSION,
        codec: crate::protocol::AudioCodec::Pcm,
        flags: if take_start { packet::FLAG_MARKER } else { 0 },
        jam_alias,
        stream_alias,
        generation,
        sequence,
        capture_timestamp: capture_ticks,
        frames: frames as u16,
        channels: channels as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AudioCodec;

    #[test]
    fn an_outbound_header_refuses_a_packet_that_cannot_be_described() {
        assert!(outbound_header(1, 1, 1, 0, 0, 70_000, 2, false).is_err());
        assert!(outbound_header(1, 1, 1, 0, 0, 128, 0, false).is_err());
        let header = outbound_header(9, 3, 2, 7, 20_000_000, 128, 2, true).expect("valid");
        assert_eq!(header.frames, 128);
        assert!(header.has(packet::FLAG_MARKER));
        assert_eq!(header.capture_timestamp, 20_000_000);
    }

    /// Records what reached the sink: (frames, channels, samples) per block.
    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<(usize, usize, usize)>>);

    impl JamAudioSink for RecordingSink {
        fn deliver(&self, _stream: &StreamId, frame: JamAudioFrame<'_>) {
            self.0.lock().expect("sink lock").push((
                frame.frames,
                frame.channels,
                frame.samples.len(),
            ));
        }
        fn stream_ended(&self, _stream: &StreamId) {}
    }

    fn pcm_stream(channels: i32) -> SubscribedStream {
        SubscribedStream {
            id: StreamId::new("str_1"),
            format: AudioFormat {
                codec: AudioCodec::Pcm,
                sample_rate: 48_000,
                channels,
                format: SampleFormat::F32Le,
                bitrate: 0,
                frame_samples: 128,
            },
            latency: LatencyMetadata::default(),
        }
    }

    fn silence_header(frames: u16, channels: u8) -> AudioPacketHeader {
        let mut header = outbound_header(1, 1, 1, 0, 0, 0, 1, false).expect("valid");
        header.flags = packet::FLAG_SILENCE;
        header.frames = frames;
        header.channels = channels;
        header
    }

    #[test]
    fn an_oversized_silence_header_is_dropped_without_a_large_allocation() {
        let recording = Arc::new(RecordingSink::default());
        let sink: Arc<dyn JamAudioSink> = recording.clone();
        let shared = MediaShared::new();
        let stream = pcm_stream(2);
        let mut decoded = Vec::new();

        // The worst case the header can describe: 65 535 frames x 255 channels.
        for (frames, channels) in [(u16::MAX, u8::MAX), (u16::MAX, 2), (8_192, 2), (1, u8::MAX)] {
            let header = silence_header(frames, channels);
            deliver(&stream, &header, &[], &sink, &mut decoded, &shared, 4);
        }
        assert!(recording.0.lock().expect("sink lock").is_empty());
        assert!(
            decoded.capacity() * std::mem::size_of::<f32>() <= packet::MAX_MEDIA_FRAME,
            "a rejected silence header grew the decode buffer to {} samples",
            decoded.capacity()
        );
    }

    #[test]
    fn a_silence_header_within_the_packet_limit_still_inserts_silence() {
        let recording = Arc::new(RecordingSink::default());
        let sink: Arc<dyn JamAudioSink> = recording.clone();
        let shared = MediaShared::new();
        let stream = pcm_stream(2);
        let mut decoded = Vec::new();

        deliver(
            &stream,
            &silence_header(128, 2),
            &[],
            &sink,
            &mut decoded,
            &shared,
            4,
        );
        // A header without a channel count falls back to the negotiated format.
        deliver(
            &stream,
            &silence_header(128, 0),
            &[],
            &sink,
            &mut decoded,
            &shared,
            4,
        );
        assert_eq!(
            *recording.0.lock().expect("sink lock"),
            vec![(128, 2, 256), (128, 2, 256)]
        );
        assert!(decoded.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn silence_samples_follow_the_media_frame_bound() {
        let max_f32 = (packet::MAX_MEDIA_FRAME - packet::HEADER_SIZE) / 4;
        assert_eq!(silence_samples(128, 2, SampleFormat::F32Le), Some(256));
        assert_eq!(
            silence_samples((max_f32 / 2) as u16, 2, SampleFormat::F32Le),
            Some(max_f32 / 2 * 2)
        );
        assert_eq!(
            silence_samples((max_f32 / 2 + 1) as u16, 2, SampleFormat::F32Le),
            None
        );
        assert_eq!(silence_samples(u16::MAX, 255, SampleFormat::S16Le), None);
        assert_eq!(silence_samples(128, 0, SampleFormat::F32Le), None);
        assert_eq!(silence_samples(128, 2, SampleFormat::None), None);
    }

    #[test]
    fn shared_state_starts_with_nothing_subscribed_and_no_loss() {
        let shared = MediaShared::new();
        assert!(shared.all_stats().is_empty());
        assert!(!shared.transport_lost.load(Ordering::Acquire));
        assert!(shared.streams.read().expect("readable").get(&1).is_none());
    }

    #[test]
    fn peaks_are_held_between_reads_rather_than_overwritten() {
        let shared = MediaShared::new();
        let id = StreamId::new("str_1");
        record_stats(&shared, &id, JitterStats::default(), Some(0.8));
        record_stats(&shared, &id, JitterStats::default(), Some(0.2));
        assert_eq!(shared.stats_for(&id).expect("present").peak, 0.8);
    }

    #[test]
    fn jitter_counters_replace_rather_than_accumulate() {
        let shared = MediaShared::new();
        let id = StreamId::new("str_1");
        record_stats(
            &shared,
            &id,
            JitterStats {
                received: 5,
                ..Default::default()
            },
            None,
        );
        record_stats(
            &shared,
            &id,
            JitterStats {
                received: 9,
                ..Default::default()
            },
            None,
        );
        assert_eq!(shared.stats_for(&id).expect("present").jitter.received, 9);
    }

    #[test]
    fn a_publication_starts_at_sequence_zero_and_marks_the_take() {
        struct Silent;
        impl JamPublishSource for Silent {
            fn pull(&self, _out: &mut Vec<f32>, _max: usize) -> Option<crate::bridge::PulledBlock> {
                None
            }
        }
        let publication = Publication::new(
            StreamId::new("str_1"),
            4,
            AudioFormat {
                codec: AudioCodec::Pcm,
                sample_rate: 48_000,
                channels: 2,
                format: SampleFormat::F32Le,
                bitrate: 0,
                frame_samples: 128,
            },
            Arc::new(Silent),
        );
        assert_eq!(publication.sequence, 0);
        assert!(publication.take_start);
    }
}
