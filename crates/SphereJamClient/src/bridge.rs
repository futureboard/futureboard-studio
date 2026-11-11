//! Where jam audio meets the host application.
//!
//! This crate deliberately owns no audio device, no resampler and no mixer. It
//! hands decoded interleaved `f32` to a sink the host provides and pulls blocks
//! from sources the host provides; Futureboard Studio implements both against
//! its existing engine, so the jam gains no second audio path and the engine
//! gains no knowledge of sockets.
//!
//! Timing is the part that has to be right here. Every delivered frame carries
//! two different timestamps and they are not interchangeable:
//!
//! * **Capture timestamp** — the session tick at which the publisher captured
//!   the first sample. It is what a recording is aligned against. Network delay
//!   does not appear in it.
//! * **Presentation timestamp** — when this receiver intends to play the frame.
//!   It carries the jitter buffer's depth and is a monitoring concern only.
//!
//! Using arrival time for either would put a remote take at a position that
//! depends on the weather of the network, which is exactly the bug the session
//! clock exists to prevent.

use crate::ids::StreamId;
use crate::protocol::{LatencyMetadata, SampleFormat};

/// How a remote stream's channels reach a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelMapping {
    /// A mono stream, or a stereo one summed to a mono destination.
    Mono,
    /// Channel 0 to the left, channel 1 to the right. The default, because a
    /// stereo stream arriving folded to mono is a surprise, and one arriving
    /// wide is what the publisher intended.
    #[default]
    Stereo,
    /// Channel 0 to both sides.
    LeftOnly,
    /// Channel 1 to both sides.
    RightOnly,
}

impl ChannelMapping {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::LeftOnly => "left",
            Self::RightOnly => "right",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "mono" => Some(Self::Mono),
            "stereo" => Some(Self::Stereo),
            "left" => Some(Self::LeftOnly),
            "right" => Some(Self::RightOnly),
            _ => None,
        }
    }

    /// What a menu entry says.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mono => "Mono",
            Self::Stereo => "Stereo",
            Self::LeftOnly => "Left",
            Self::RightOnly => "Right",
        }
    }

    /// The mappings that make sense for a stream with this many channels.
    pub fn options_for(channels: usize) -> &'static [ChannelMapping] {
        if channels >= 2 {
            &[
                ChannelMapping::Stereo,
                ChannelMapping::LeftOnly,
                ChannelMapping::RightOnly,
                ChannelMapping::Mono,
            ]
        } else {
            &[ChannelMapping::Mono]
        }
    }

    /// Resolve one interleaved source frame to a stereo destination pair.
    ///
    /// `source` is one frame of `channels` interleaved samples. A stream with
    /// fewer channels than the mapping expects falls back to what it has rather
    /// than reading past the frame.
    #[inline]
    pub fn resolve(self, source: &[f32], channels: usize) -> (f32, f32) {
        if channels == 0 || source.is_empty() {
            return (0.0, 0.0);
        }
        let left = source[0];
        let right = if channels >= 2 && source.len() >= 2 {
            source[1]
        } else {
            left
        };
        match self {
            Self::Stereo => (left, right),
            Self::LeftOnly => (left, left),
            Self::RightOnly => (right, right),
            // A sum, not an average of one side: two correlated channels summed
            // and halved is the conventional fold-down and keeps the level.
            Self::Mono => {
                if channels >= 2 {
                    let sum = (left + right) * 0.5;
                    (sum, sum)
                } else {
                    (left, left)
                }
            }
        }
    }
}

/// One block of decoded remote audio, borrowed from the receive thread's own
/// buffer. Nothing here is owned, so delivery allocates nothing.
#[derive(Debug, Clone, Copy)]
pub struct JamAudioFrame<'a> {
    /// Session tick of the first sample, as captured by the publisher. The
    /// anchor for recording alignment; never the arrival time.
    pub capture_timestamp: u64,
    /// Session tick at which this receiver intends to play the frame — the
    /// capture tick plus the depth this receiver is buffering. Monitoring only.
    pub presentation_timestamp: u64,
    pub sequence: u64,
    /// Samples per channel in this block.
    pub frames: usize,
    pub channels: usize,
    pub sample_rate: u32,
    /// Packet flags: marker, silence, end of stream, redundant.
    pub flags: u16,
    /// Interleaved `frames * channels` samples.
    pub samples: &'a [f32],
    /// The publisher's reported fixed delays, forwarded untouched by the server.
    pub latency: LatencyMetadata,
}

impl JamAudioFrame<'_> {
    /// The capture tick the next packet of a continuous stream should carry.
    ///
    /// The tick domain and the stream rate are not the same thing: a 96 kHz
    /// stream advances two samples per tick, so a frame count has to be
    /// converted before it can be added to a tick. Getting this wrong makes a
    /// high-rate stream appear to drift at exactly the ratio between the rates,
    /// which is the sort of bug that looks like a network problem.
    pub fn next_capture_timestamp(&self, clock_rate: u32) -> u64 {
        if self.sample_rate == 0 || clock_rate == 0 {
            return self.capture_timestamp;
        }
        let ticks = self.frames as u128 * clock_rate as u128 / self.sample_rate as u128;
        self.capture_timestamp.saturating_add(ticks as u64)
    }

    /// Whether this block is a transmit gap the receiver should fill with
    /// silence rather than conceal as a loss.
    pub fn is_silence(&self) -> bool {
        self.flags & crate::packet::FLAG_SILENCE != 0
    }

    /// Whether this is the first packet of a take.
    pub fn is_take_start(&self) -> bool {
        self.flags & crate::packet::FLAG_MARKER != 0
    }
}

/// Receives decoded remote audio.
///
/// Called from the media receive thread, never from an audio callback. An
/// implementation must not block: the expected shape is a copy into a
/// preallocated lock-free ring the audio thread drains.
pub trait JamAudioSink: Send + Sync {
    /// Deliver one decoded block.
    fn deliver(&self, stream: &StreamId, frame: JamAudioFrame<'_>);

    /// A stream this sink may have been writing to is gone. The sink should
    /// release whatever it allocated for it and go silent.
    fn stream_ended(&self, stream: &StreamId);
}

/// A sink that discards everything, for a client with no audio host attached.
pub struct NullSink;

impl JamAudioSink for NullSink {
    fn deliver(&self, _stream: &StreamId, _frame: JamAudioFrame<'_>) {}
    fn stream_ended(&self, _stream: &StreamId) {}
}

/// What one pull produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PulledBlock {
    /// Samples per channel written.
    pub frames: usize,
    /// Session tick of the first sample written.
    pub capture_ticks: u64,
    /// Whether this is the first block of a take.
    pub take_start: bool,
}

/// Supplies audio to publish.
///
/// The host pulls from wherever the publish source is tapped — a track, a bus,
/// the master, a hardware input — already converted to the negotiated rate and
/// channel count. Conversion belongs to the host because the host owns the
/// resampler the rest of Studio uses; duplicating one here would mean two
/// different sounds for the same signal.
pub trait JamPublishSource: Send + Sync {
    /// Fill `out` with up to `max_frames` interleaved frames.
    ///
    /// `None` means nothing is available yet, which is normal: the publish
    /// worker simply tries again rather than sending silence, so a stopped
    /// transport costs no bandwidth.
    fn pull(&self, out: &mut Vec<f32>, max_frames: usize) -> Option<PulledBlock>;

    /// The fixed delays this source reports, sent with the stream so receivers
    /// can align a take. Unknown values must be left at zero rather than
    /// guessed: a wrong number moves a waveform, a zero merely fails to.
    fn latency(&self) -> LatencyMetadata {
        LatencyMetadata::default()
    }
}

/// What Studio is publishing from.
///
/// The variants name the audio graph node, not a device. A jam publish is a tap
/// on something the engine is already rendering, which is what keeps the jam
/// from opening a second capture client and fighting the DAW for the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JamPublishSourceKind {
    /// A hardware input, as the engine already captures it.
    HardwareInput {
        connection: String,
    },
    Track {
        track_id: String,
    },
    Bus {
        track_id: String,
    },
    Master,
    /// Several tracks as one wide stream, one channel pair each.
    ///
    /// One stream rather than one per track, because a receiving Studio is
    /// dropping a take onto a timeline: a single stream carries one capture
    /// base and one sequence, so every pair lands sample-aligned with the rest.
    /// N stereo streams would arrive on N independently jittered paths and have
    /// to be re-aligned against each other, which is the problem the session
    /// clock exists to avoid rather than to multiply.
    Multitrack {
        track_ids: Vec<String>,
    },
}

impl JamPublishSourceKind {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::HardwareInput { .. } => "hardware_input",
            Self::Track { .. } => "track",
            Self::Bus { .. } => "bus",
            Self::Master => "master",
            Self::Multitrack { .. } => "multitrack",
        }
    }
}

/// A stream this client wants to publish.
///
/// The wire format is per stream rather than per session because the two are
/// genuinely independent: a Studio can send its master as 24-bit stereo and its
/// arrangement as 16-bit sixteen-channel at the same time, and the packet a
/// receiver decodes is described by the stream it belongs to.
#[derive(Debug, Clone)]
pub struct JamPublishRequest {
    /// The display name other participants see, e.g. `Guitar`.
    pub name: String,
    pub source: JamPublishSourceKind,
    pub channels: usize,
    /// Per-channel labels. Empty lets the server and receivers fall back to the
    /// layout convention.
    pub channel_labels: Vec<String>,
    /// Wire sample format. Deeper is more bandwidth, not more latency.
    pub sample_format: SampleFormat,
    /// Rate to publish at, or `0` to take the session's.
    ///
    /// Independent of the project rate either way: the publish tap converts.
    pub sample_rate: i32,
    /// Frame sizes offered for this stream alone. Empty leaves the session's
    /// advertised capabilities in charge.
    ///
    /// A wide layout has to state one: sixteen channels at 256 samples is 16 kB
    /// of PCM, which no datagram carries, and the server would refuse the
    /// stream rather than negotiate a size neither side offered.
    pub frame_sizes: Vec<i32>,
}

impl JamPublishRequest {
    pub fn stereo(name: impl Into<String>, source: JamPublishSourceKind) -> Self {
        Self {
            name: name.into(),
            source,
            channels: 2,
            channel_labels: vec!["L".to_string(), "R".to_string()],
            sample_format: SampleFormat::F32Le,
            sample_rate: 0,
            frame_sizes: Vec::new(),
        }
    }

    pub fn mono(name: impl Into<String>, source: JamPublishSourceKind) -> Self {
        Self {
            name: name.into(),
            source,
            channels: 1,
            channel_labels: vec!["Mono".to_string()],
            sample_format: SampleFormat::F32Le,
            sample_rate: 0,
            frame_sizes: Vec::new(),
        }
    }

    /// A wide stream, one channel pair per entry in `pair_labels`.
    ///
    /// The frame size is derived rather than asked for: it is the largest
    /// advertised size whose PCM frame still fits one datagram at this layout
    /// and depth, which is the only choice that both negotiates and keeps the
    /// packet rate as low as the layout allows.
    pub fn multitrack(
        name: impl Into<String>,
        track_ids: Vec<String>,
        pair_labels: &[String],
        sample_format: SampleFormat,
    ) -> Self {
        let channels = track_ids.len() * 2;
        let mut channel_labels = Vec::with_capacity(channels);
        for pair in 0..track_ids.len() {
            let label = pair_labels
                .get(pair)
                .filter(|label| !label.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Ch {}", pair + 1));
            channel_labels.push(format!("{label} L"));
            channel_labels.push(format!("{label} R"));
        }
        Self {
            name: name.into(),
            source: JamPublishSourceKind::Multitrack { track_ids },
            channels,
            channel_labels,
            sample_format,
            sample_rate: 0,
            // One size, not a list: the server takes the smallest a publisher
            // offers, and for a wide layout the smallest that fits is already a
            // punishing packet rate. Offering only the largest that fits is how
            // this stream asks for the lowest packet rate its layout allows.
            frame_sizes: datagram_frame_sizes(channels, sample_format)
                .into_iter()
                .take(1)
                .collect(),
        }
    }
}

/// Payload bytes one datagram carries. The server's own
/// `media.DefaultMaxPayloadBytes`, restated here because the value bounds what
/// this client may offer before any handshake has happened.
pub const DATAGRAM_PAYLOAD_BYTES: usize = 1200;

/// Every conventional frame size whose PCM frame fits one datagram at this
/// layout and depth, largest first.
///
/// Largest first because the server's negotiator takes the smallest size both
/// sides offer, and for a wide layout the smallest that fits is already a very
/// high packet rate — 32 samples at 48 kHz is 1500 packets a second. Listing
/// the sizes at all is what makes the stream negotiable; the caller decides how
/// far down the list to go.
pub fn datagram_frame_sizes(channels: usize, format: SampleFormat) -> Vec<i32> {
    let per_frame = channels.max(1) * format.bytes_per_sample().max(1);
    [512, 256, 128, 64, 32]
        .into_iter()
        .filter(|size| (*size as usize) * per_frame <= DATAGRAM_PAYLOAD_BYTES)
        .collect()
}

/// A project-persisted routing from a remote performer to a local track.
///
/// It is keyed on the account, not on the stream: stream ids are minted per
/// session, so a project reopened tomorrow would find every id stale. The
/// stream id is kept as the exact match to try first, the name is what makes a
/// later session re-bind, and the device narrows it when one account is on
/// several machines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JamTrackBinding {
    pub user_id: crate::ids::UserId,
    pub preferred_stream_id: Option<StreamId>,
    pub preferred_stream_name: Option<String>,
    pub preferred_device_id: Option<crate::ids::DeviceId>,
    pub channel_mapping: ChannelMapping,
    /// Whether to attach automatically when the performer appears.
    pub auto_connect: bool,
}

impl JamTrackBinding {
    /// What a track's Input field shows while the performer is not in the room.
    pub fn waiting_label(&self, display: Option<&str>) -> String {
        let who = display.unwrap_or(self.user_id.as_str());
        match &self.preferred_stream_name {
            Some(name) => format!("Waiting for {who} · {name}"),
            None => format!("Waiting for {who}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_mapping_keeps_the_sides_apart() {
        let (l, r) = ChannelMapping::Stereo.resolve(&[0.25, -0.5], 2);
        assert_eq!((l, r), (0.25, -0.5));
    }

    #[test]
    fn single_side_mappings_feed_both_outputs() {
        assert_eq!(
            ChannelMapping::LeftOnly.resolve(&[0.25, -0.5], 2),
            (0.25, 0.25)
        );
        assert_eq!(
            ChannelMapping::RightOnly.resolve(&[0.25, -0.5], 2),
            (-0.5, -0.5)
        );
    }

    #[test]
    fn a_mono_fold_down_halves_the_sum_rather_than_dropping_a_side() {
        let (l, r) = ChannelMapping::Mono.resolve(&[1.0, 0.0], 2);
        assert_eq!(l, 0.5);
        assert_eq!(r, 0.5);
    }

    #[test]
    fn a_mono_stream_is_centred_whatever_the_mapping_asks_for() {
        for mapping in [
            ChannelMapping::Stereo,
            ChannelMapping::Mono,
            ChannelMapping::LeftOnly,
            ChannelMapping::RightOnly,
        ] {
            assert_eq!(mapping.resolve(&[0.75], 1), (0.75, 0.75), "{mapping:?}");
        }
    }

    #[test]
    fn an_empty_frame_is_silence_not_an_index_panic() {
        assert_eq!(ChannelMapping::Stereo.resolve(&[], 2), (0.0, 0.0));
        assert_eq!(ChannelMapping::Stereo.resolve(&[0.5], 0), (0.0, 0.0));
    }

    #[test]
    fn a_mono_stream_offers_only_the_mono_mapping() {
        assert_eq!(ChannelMapping::options_for(1), &[ChannelMapping::Mono]);
        assert_eq!(ChannelMapping::options_for(2).len(), 4);
    }

    #[test]
    fn mapping_tags_round_trip_for_project_persistence() {
        for mapping in [
            ChannelMapping::Stereo,
            ChannelMapping::Mono,
            ChannelMapping::LeftOnly,
            ChannelMapping::RightOnly,
        ] {
            assert_eq!(ChannelMapping::from_tag(mapping.tag()), Some(mapping));
        }
        assert_eq!(ChannelMapping::from_tag("surround"), None);
    }

    #[test]
    fn capture_and_presentation_timestamps_are_separate_values() {
        let samples = [0.0f32; 4];
        let frame = JamAudioFrame {
            capture_timestamp: 20_000_000,
            presentation_timestamp: 20_000_384,
            sequence: 5,
            frames: 2,
            channels: 2,
            sample_rate: 48_000,
            flags: crate::packet::FLAG_MARKER,
            samples: &samples,
            latency: LatencyMetadata::default(),
        };
        assert_ne!(frame.capture_timestamp, frame.presentation_timestamp);
        // Two frames of a 48 kHz stream are two ticks of a 48 kHz clock.
        assert_eq!(frame.next_capture_timestamp(48_000), 20_000_002);
        assert!(frame.is_take_start());
        assert!(!frame.is_silence());
    }

    #[test]
    fn a_high_rate_stream_advances_the_tick_clock_by_fewer_ticks_than_frames() {
        let samples = [0.0f32; 8];
        let frame = JamAudioFrame {
            capture_timestamp: 1_000,
            presentation_timestamp: 1_000,
            sequence: 0,
            frames: 4,
            channels: 2,
            // Four frames at 96 kHz are two ticks of the 48 kHz session clock.
            sample_rate: 96_000,
            flags: 0,
            samples: &samples,
            latency: LatencyMetadata::default(),
        };
        assert_eq!(frame.next_capture_timestamp(48_000), 1_002);
    }

    #[test]
    fn a_waiting_binding_names_the_performer_and_the_stream() {
        let binding = JamTrackBinding {
            user_id: crate::ids::UserId::new("usr_1"),
            preferred_stream_id: None,
            preferred_stream_name: Some("Guitar".to_string()),
            preferred_device_id: None,
            channel_mapping: ChannelMapping::Stereo,
            auto_connect: true,
        };
        assert_eq!(
            binding.waiting_label(Some("@hachi224")),
            "Waiting for @hachi224 · Guitar"
        );
    }
}
