//! Media add-ons and flavors (BeOS `BMediaAddOn` / `flavor_info`), and the codec plugin
//! interfaces add-ons instantiate.
//!
//! An add-on advertises *flavors*: things it can make, with the formats they take and produce,
//! the node kinds they implement and — the MKFF addition — where they run ([`Acceleration`]).
//! The roster ranks flavors by that and by [`FlavorInfo::rank`], so asking for "an H.264 decoder"
//! gives the GPU's decoder when there is one and the software decoder otherwise, the same way the
//! BeOS media server picked the best add-on for a format.

use std::fmt;

use crate::buffer::GpuSurface;
use crate::format::{MediaFormat, PixelFormat, RawAudioFormat, RawVideoFormat};
use crate::node::MediaNode;
use crate::Result;

/// Node kinds (`node_kind`), as bit flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Kinds(pub u32);

impl Kinds {
    pub const BUFFER_PRODUCER: Kinds = Kinds(1);
    pub const BUFFER_CONSUMER: Kinds = Kinds(2);
    pub const TIME_SOURCE: Kinds = Kinds(4);
    pub const CONTROLLABLE: Kinds = Kinds(8);
    pub const FILE_INTERFACE: Kinds = Kinds(16);
    pub const ENTITY_INTERFACE: Kinds = Kinds(32);
    pub const PHYSICAL_INPUT: Kinds = Kinds(0x1_0000);
    pub const PHYSICAL_OUTPUT: Kinds = Kinds(0x2_0000);
    pub const SYSTEM_MIXER: Kinds = Kinds(0x4_0000);
    /// Codec flavors (not BeOS node kinds: the BeOS codec kit kept them separate).
    pub const DECODER: Kinds = Kinds(0x100_0000);
    pub const ENCODER: Kinds = Kinds(0x200_0000);

    pub fn contains(self, other: Kinds) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Kinds {
    type Output = Kinds;
    fn bitor(self, o: Kinds) -> Kinds {
        Kinds(self.0 | o.0)
    }
}

/// GPU vendors, from the PCI vendor id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Qualcomm,
    Microsoft,
    Other(u32),
}

impl GpuVendor {
    pub fn from_pci(id: u32) -> GpuVendor {
        match id {
            0x10DE => GpuVendor::Nvidia,
            0x1002 | 0x1022 => GpuVendor::Amd,
            0x8086 => GpuVendor::Intel,
            0x5143 | 0x4D4F4351 => GpuVendor::Qualcomm,
            0x1414 => GpuVendor::Microsoft,
            other => GpuVendor::Other(other),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            GpuVendor::Nvidia => "NVIDIA",
            GpuVendor::Amd => "AMD",
            GpuVendor::Intel => "Intel",
            GpuVendor::Qualcomm => "Qualcomm",
            GpuVendor::Microsoft => "Microsoft",
            GpuVendor::Other(_) => "GPU",
        }
    }
}

/// Where a flavor does its work.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Acceleration {
    /// On the CPU.
    #[default]
    Software,
    /// On a GPU's fixed-function media engine (through an [`crate::accelerant::Accelerant`]).
    Gpu {
        vendor: GpuVendor,
        adapter_luid: u64,
        /// The driver path, e.g. "D3D11 Video (DXVA)", "Media Foundation (NVENC)".
        api: String,
    },
}

impl Acceleration {
    pub fn is_hardware(&self) -> bool {
        !matches!(self, Acceleration::Software)
    }
}

impl fmt::Display for Acceleration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Acceleration::Software => write!(f, "software"),
            Acceleration::Gpu { vendor, api, .. } => write!(f, "{} {}", vendor.name(), api),
        }
    }
}

/// One thing an add-on can instantiate (`flavor_info`).
#[derive(Clone, Debug)]
pub struct FlavorInfo {
    /// Unique within the add-on.
    pub id: u32,
    pub name: String,
    pub info: String,
    pub kinds: Kinds,
    pub accel: Acceleration,
    /// Formats it consumes (decoders: encoded; encoders: raw).
    pub in_formats: Vec<MediaFormat>,
    /// Formats it produces.
    pub out_formats: Vec<MediaFormat>,
    /// Preference among flavors with the same acceleration class (higher first).
    pub rank: i32,
}

/// Decoded audio from a decoder plugin: planar f32, WAV channel order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioChunk {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
    /// Media time of the first sample in µs (the packet's timestamp, passed through).
    pub pts: i64,
}

impl AudioChunk {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

/// A decoded picture: CPU planes, a GPU surface, or both.
#[derive(Clone, Debug, Default)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Media time in µs (the packet timestamp it came from).
    pub pts: i64,
    /// CPU planes (empty when the frame only lives on the GPU).
    pub planes: Vec<Vec<u8>>,
    pub strides: Vec<usize>,
    pub gpu: Option<GpuSurface>,
}

/// Compressed output of an encoder plugin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncodedChunk {
    pub data: Vec<u8>,
    /// Timestamps in µs.
    pub pts: i64,
    pub dts: i64,
    pub duration: i64,
    pub keyframe: bool,
}

/// An audio decoder instantiated from a flavor (BeOS codec-kit `Decoder` for audio).
pub trait AudioDecoderPlugin: Send {
    /// Decode one packet. `pts` (µs) is echoed on the output.
    fn decode(&mut self, packet: &[u8], pts: i64) -> Result<AudioChunk>;
    /// Forget state after a seek (`SeekedTo`).
    fn reset(&mut self);
    /// What comes out (rate / channels may be 0 until the first packet).
    fn output_format(&self) -> RawAudioFormat;
}

/// A video decoder instantiated from a flavor. Packets go in in decode order, frames come out in
/// presentation order.
pub trait VideoDecoderPlugin: Send {
    fn decode(&mut self, packet: &[u8], pts: i64) -> Result<Vec<VideoFrame>>;
    /// Drain frames held for reordering.
    fn flush(&mut self) -> Result<Vec<VideoFrame>>;
    fn reset(&mut self);
    fn output_format(&self) -> RawVideoFormat;
    /// Where this instance actually runs.
    fn acceleration(&self) -> Acceleration {
        Acceleration::Software
    }
}

/// An audio encoder instantiated from a flavor.
pub trait AudioEncoderPlugin: Send {
    /// Planar f32 in (one slice per channel).
    fn encode(&mut self, planar: &[&[f32]]) -> Result<Vec<EncodedChunk>>;
    fn flush(&mut self) -> Result<Vec<EncodedChunk>>;
    /// The encoded format, including the decoder configuration a container needs.
    fn output_format(&self) -> MediaFormat;
}

/// A video encoder instantiated from a flavor.
pub trait VideoEncoderPlugin: Send {
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<EncodedChunk>>;
    fn flush(&mut self) -> Result<Vec<EncodedChunk>>;
    fn output_format(&self) -> MediaFormat;
    fn acceleration(&self) -> Acceleration {
        Acceleration::Software
    }
}

/// What [`MediaAddOn::instantiate`] returns.
pub enum Instance {
    AudioDecoder(Box<dyn AudioDecoderPlugin>),
    VideoDecoder(Box<dyn VideoDecoderPlugin>),
    AudioEncoder(Box<dyn AudioEncoderPlugin>),
    VideoEncoder(Box<dyn VideoEncoderPlugin>),
    Node(Box<dyn MediaNode>),
}

impl fmt::Debug for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Instance::AudioDecoder(_) => "Instance::AudioDecoder",
            Instance::VideoDecoder(_) => "Instance::VideoDecoder",
            Instance::AudioEncoder(_) => "Instance::AudioEncoder",
            Instance::VideoEncoder(_) => "Instance::VideoEncoder",
            Instance::Node(_) => "Instance::Node",
        })
    }
}

/// A media add-on (`BMediaAddOn`): a provider of flavors.
pub trait MediaAddOn: Send + Sync {
    fn name(&self) -> &str;
    fn flavors(&self) -> Vec<FlavorInfo>;
    /// Make an instance of flavor `flavor_id`. `input` is the concrete input format (decoders:
    /// the stream's encoded format with its configuration; encoders: the raw format);
    /// `output` is the requested output (encoders: codec, bit rate in
    /// [`crate::format::EncodedFormat::bit_rate`]; decoders: usually a wildcard).
    fn instantiate(
        &self,
        flavor_id: u32,
        input: &MediaFormat,
        output: &MediaFormat,
    ) -> Result<Instance>;
}
