//! # MKFF Media Kit
//!
//! A BeOS-style Media Kit for MKFF, in-process, with GPU acceleration on Windows.
//!
//! | BeOS | here |
//! |------|------|
//! | `BMediaRoster` (talking to `media_server`) | [`MediaRoster`]: add-ons, nodes, connections, codec selection |
//! | `BMediaAddOn`, `flavor_info`, dormant nodes | [`MediaAddOn`], [`FlavorInfo`] (+ where it runs: [`Acceleration`]) |
//! | codec kit `Decoder` / `Encoder` plugins | [`AudioDecoderPlugin`], [`VideoDecoderPlugin`], [`AudioEncoderPlugin`], [`VideoEncoderPlugin`] |
//! | `BMediaNode` + `BBufferProducer` / `BBufferConsumer` + `BMediaEventLooper` | [`MediaNode`]: one thread and timed event queue per node |
//! | `media_format` + `FormatProposal` / `AcceptFormat` | [`MediaFormat`] with wildcard matching and specialisation |
//! | `BBuffer`, `BBufferGroup`, `media_header`, areas | [`Buffer`], [`BufferGroup`], [`MediaHeader`], [`Area`] (shared memory) |
//! | `BTimeSource` | [`TimeSource`]: performance ↔ real time, published by the master clock |
//! | graphics accelerants (`get_accelerant_hook`, engines, sync tokens) | [`accelerant`]: one [`Accelerant`] per GPU (DXGI / D3D11 / DXVA / Media Foundation) |
//!
//! Codec selection asks the roster for the best flavor: GPU flavors (from accelerants) first,
//! software flavors (registered by codec crates such as `mkff-audio`) as the fallback, under an
//! [`AccelPolicy`] (`MKFF_ACCEL=auto|hw|sw`).
//!
//! ```no_run
//! use mkff_mediakit::{AccelPolicy, CodecFamily, MediaFormat, MediaRoster};
//!
//! let roster = MediaRoster::roster();
//! for a in roster.accelerants() {
//!     println!("{}", a.device_info().name);
//! }
//! let h264 = MediaFormat::encoded(CodecFamily::H264, Vec::new(), 1920, 1080);
//! let (flavor, mut dec) = roster.video_decoder(&h264, AccelPolicy::Auto)?;
//! println!("decoding with {} ({})", flavor.info.name, flavor.info.accel);
//! # let _ = dec.flush();
//! # Ok::<(), mkff_mediakit::Error>(())
//! ```

pub mod accelerant;
pub mod addon;
pub mod area;
pub mod buffer;
mod error;
pub mod format;
pub mod node;
pub mod nodes;
pub mod roster;
pub mod time_source;

pub use accelerant::{Accelerant, DeviceInfo, Feature, HookInfo, SyncToken};
pub use addon::{
    Acceleration, AudioChunk, AudioDecoderPlugin, AudioEncoderPlugin, EncodedChunk, FlavorInfo,
    GpuVendor, Instance, Kinds, MediaAddOn, VideoDecoderPlugin, VideoEncoderPlugin, VideoFrame,
};
pub use area::Area;
pub use buffer::{Buffer, BufferGroup, GpuSurface, MediaHeader, VideoLayout};
pub use error::{Error, Result};
pub use format::{
    CodecFamily, EncodedFormat, MediaFormat, PixelFormat, RawAudioFormat, RawVideoFormat,
    SampleFormat,
};
pub use node::{
    DataStatus, MediaDestination, MediaEvent, MediaInput, MediaNode, MediaOutput, MediaSource,
    NodeContext, NodeId, RunState, IMMEDIATE,
};
pub use roster::{AccelPolicy, AddOnId, Connection, FlavorRef, MediaRoster, NodeRef};
pub use time_source::TimeSource;
