<p align="center">
  <img src="assets/banner.png" alt="MediaKit Foundation" width="720">
</p>

<p align="center">
  <a href="https://github.com/futureboard/mediakit-foundation/actions/workflows/build.yml"><img src="https://github.com/futureboard/mediakit-foundation/actions/workflows/build.yml/badge.svg" alt="build"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="license"></a>
  <img src="https://img.shields.io/badge/platforms-Linux%20%7C%20Windows%20%7C%20macOS-lightgrey.svg" alt="platforms">
  <img src="https://img.shields.io/badge/C-11-00599C.svg" alt="C11">
  <img src="https://img.shields.io/badge/Rust-bindings-orange.svg" alt="Rust">
</p>

# MediaKit Foundation Framework (MKFF)

Cross-Platform Media Engine, Inspired by BeOS.

> Status: **0.1.0-dev**. H.264 and HEVC (Main / Main10) decode on Linux
> (VA-API), Windows (D3D11 Video Decode), and macOS (VideoToolbox), plus
> an optional Apache-2.0 libhevc software fallback for HEVC. Progressive
> MP4/MOV demux (`avc1` / `hvc1` / `hev1` → Annex-B).
>
> **Every other codec, in software, without FFmpeg:** [`mkff-codecs`](#software-codecs-mkff-codecs)
> (Rust) and `libmkff_swcodecs` ([`mkff/codecs.h`](include/mkff/codecs.h), C) decode
> MP4/MOV, MKV/WebM, AVI, MXF, MPEG-TS/PS and Ogg with H.264, HEVC Main/Main10,
> VP8, VP9, AV1, MPEG-1/2, ProRes, DNxHD/HR, APV and MJPEG video plus AAC,
> Opus, AC-3/E-AC-3, MP1/2/3, FLAC, ALAC and Vorbis audio. Video is adapted from
> the pure-Rust [FilmCraft](https://github.com/storytold/filmcraft/tree/main/crates)
> codec crates (being replaced step by step).
>
> **MKFF's own audio stack:** [`mkff-audio`](#mkff-audio-mkff-audio) decodes MP3,
> AAC, FLAC, ALAC, AC-3/E-AC-3, Opus, Vorbis and PCM and encodes MP3, AAC, FLAC,
> ALAC and WAV, with its own WAV / AIFF / MP3 / ADTS / FLAC / Ogg / MP4 demuxers and
> muxers. All of it is written from the specifications, with no FilmCraft or
> symphonia, and `mkff-codecs` uses it for all audio it demuxes itself.
>
> **[Media Kit](#media-kit-mkff-mediakit):** a BeOS-style Media Kit (`mkff-mediakit`):
> media roster, add-ons / flavors, event-looper nodes, shared-memory buffer groups,
> time sources, and *accelerants* — one per GPU — that put DXVA decode and
> NVENC / Quick Sync / AMF encode behind the same codec selection as the software
> codecs on Windows.
>
> **[MKFF Player](#mkff-player-mkff-player):** a desktop media player on egui / wgpu with
> audio-master A/V sync, seeking, GPU Y'CbCr → RGB and surround downmix, for every file
> `mkff-codecs` opens.
>
> [`.github/workflows/build.yml`](.github/workflows/build.yml) builds and
> runs the full CTest suite on all three platforms
> (`ubuntu-latest`/`macos-latest`/`windows-latest`) on every push.
> CI runners typically have no GPU HEVC, so hardware decode fails
> cleanly; with `MKFF_ENABLE_HEVC_SOFTWARE` (default ON) the software
> path is exercised end-to-end against checked-in Annex-B fixtures.

## What this is

MKFF is a portable media engine core (`libmkff`) with dynamically-loaded
platform backends (`libmkff_platform_linux` / `_macos` / `_windows`).
The core is C11, has a stable versioned C ABI, and keeps hardware frames
GPU-native (zero-copy export). CPU NV12 / P010 planes are available via
`mkff_video_frame_map_cpu_planes` for software frames and for hardware
frames when the platform implements readback (D3D11 staging / CVPixelBuffer
lock / VA derive-map).

```
MP4:     progressive moov+mdat (avc1 / hvc1 / hev1) -> Annex-B AUs
Linux:   Annex-B -> parser -> VA-API VLD -> NV12/P010 surface -> DMA-BUF
Windows: Annex-B -> parser -> D3D11/DXVA VLD -> NV12/P010 texture -> shared handle
macOS:   Annex-B -> parser -> VideoToolbox -> IOSurface-backed CVPixelBuffer
HEVC SW: Annex-B -> in-core libhevc wrapper -> CPU NV12 (Main) / P010 (Main10)
```

**Backend selection** (`MKFF_VideoBackend` on `MKFF_VideoDecoderDesc`):

| Value | Behavior |
|-------|----------|
| `AUTO` (default) | Try platform HW; on failure fall back to software HEVC if built |
| `HARDWARE_ONLY` | Platform HW only; fail with `CODEC_UNAVAILABLE` if unsupported |
| `SOFTWARE_ONLY` | In-core libhevc only (when `MKFF_ENABLE_HEVC_SOFTWARE` is ON) |

H.264 paths are unchanged and remain hardware-oriented (no software H.264).

## Layout

```
include/mkff/            public C API (portable + platform extensions)
src/codecs/h264/         portable H.264 Annex-B/SPS/PPS/slice/POC parser
src/codecs/hevc/         portable HEVC VPS/SPS/PPS/slice/POC parser
src/codecs/hevc/software libhevc C wrapper (optional software fallback)
src/core/                libmkff: context, loader, backend selection, SW glue
src/demux/mp4/           progressive MP4/MOV demux (avc1 / hvc1 / hev1)
src/platform/linux/      VA-API, DMA-BUF, H.264 + HEVC DPB
src/platform/windows/    D3D11/DXVA, shared handles, H.264 + HEVC DPB
src/platform/macos/      VideoToolbox, IOSurface export
src/cli/                 mkff CLI
include/mkff/codecs.h    C API of libmkff_swcodecs (software codecs)
bindings/rust/mkff-sys   raw FFI
bindings/rust/mkff       safe RAII wrapper (incl. Mp4Demux)
bindings/rust/mkff-codecs       pure-Rust demux + decode of every codec (audio: mkff-audio; video: FilmCraft)
bindings/rust/mkff-codecs-capi  libmkff_swcodecs: C ABI for mkff-codecs
bindings/rust/mkff-vk    dma-buf -> VkImage (Linux)
bindings/rust/mkff-wgpu  decode → map_cpu_planes → wgpu / egui player
bindings/rust/mkff-player  MKFF Player: desktop media player (egui / wgpu, A/V sync)
bindings/rust/mkff-cli     mkfx: FFmpeg-style command-line tool (probe, transcode, remux)
bindings/rust/mkff-hwenc   hardware H.264 encode (NVENC / QSV / AMF) + Media Kit add-on
bindings/rust/mkff-mediakit  BeOS-style Media Kit: roster, add-ons, nodes, buffers, accelerants
bindings/rust/mkff-audio   MKFF's audio stack: AudioFile, AudioWriter, codec registry, add-on
bindings/rust/mkff-audio-core    bit I/O, FFT / MDCT, CRCs, resampler, codec traits
bindings/rust/mkff-audio-format  WAV / AIFF / MP3 / ADTS / FLAC / Ogg / MP4 demux + mux
bindings/rust/mkff-{mp3,aac,flac,alac,ac3,opus,vorbis}  the codecs
tests/                   CTest suite
testdata/                tiny Annex-B + progressive .mp4 fixtures
testdata/codecs/         one tiny clip per container / codec (mkff-codecs)
testdata/audio/          per-codec audio fixtures + reference decodes (README in each)
```

## Requirements

- C11 compiler: Clang (Linux/macOS) or MSVC (Windows)
- CMake >= 3.20, Ninja
- Linux: `libva`, `libva-drm`, `libdrm` development packages
- Windows: Windows SDK (D3D11/DXGI/DXVA)
- macOS: Xcode command line tools
- Rust toolchain (stable) for bindings
- Network at CMake configure time when fetching libhevc (or vendored
  `third_party/libhevc/`)

No FFmpeg, GStreamer, or GPL/LGPL dependency is used in the product.
(`scripts/gen_codec_fixtures.sh` uses an ffmpeg binary only to *encode* the
test fixtures in `testdata/codecs/`; nothing links or runs it.)

See [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) for Apache-2.0 libhevc
attribution when the software fallback is enabled.

## Rust crate (Cargo git dependency)

```toml
[dependencies]
mkff = { git = "https://github.com/futureboard/mediakit-foundation" }
```

Default features: `bundled`, `hevc`, `hevc-software-fallback`.

| Feature | Effect |
|---------|--------|
| `bundled` | Build native libs via CMake during `cargo build` |
| `hevc` | `MKFF_ENABLE_HEVC=ON` (parser + HW paths) |
| `hevc-software-fallback` | Also `MKFF_ENABLE_HEVC_SOFTWARE=ON` (libhevc) |

Disable software fallback:

```toml
mkff = { git = "...", default-features = false, features = ["bundled", "hevc"] }
```

Helpers: `Context::video_decoder_hevc()`, `video_decoder_hevc_with_backend()`,
`Context::video_decoder()`, `Mp4Demux`, `VideoBackend`, `PixelFormat::P010`,
`VideoFrame::map_cpu_planes()`.

## Building

```sh
# Linux
cmake --preset linux-clang-debug
cmake --build --preset linux-clang-debug
ctest --preset linux-clang-debug

# macOS
cmake --preset macos-clang-debug
cmake --build --preset macos-clang-debug
ctest --preset macos-clang-debug

# Windows (Ninja + cl on PATH, e.g. Developer Command Prompt / vcvars)
cmake --preset windows-msvc-debug
cmake --build --preset windows-msvc-debug
ctest --preset windows-msvc-debug

# Rust
cargo build -p mkff
cargo test -p mkff
```

CMake options:

- `MKFF_ENABLE_HEVC` (default ON)
- `MKFF_ENABLE_HEVC_SOFTWARE` (default ON) — FetchContent libhevc

## CLI

```sh
mkff devices                                              # Linux only
mkff va-info                                              # Linux only
mkff decode-test input.h264 --frames 120
mkff decode-test input.hevc --codec hevc --backend auto --frames 120
mkff decode-test input.hevc --codec hevc --backend sw --frames 10
mkff export-test input.h264 --frames 10                   # Linux only
mkff benchmark input.hevc --codec hevc --seconds 10
mkff codec-info hevc --backend sw
```

## MKFF audio (`mkff-audio`)

MKFF's own audio codecs and containers, in safe Rust with no dependencies outside
the workspace. Each codec is its own crate, written from its specification and
checked against FFmpeg / libopus / libvorbis decodes of the fixtures in
`testdata/audio/` (SNR is per channel; "bit-exact" means identical integer
samples):

| Codec | Crate | Decode (vs reference) | Encode |
|-------|-------|-----------------------|--------|
| MP3 (MPEG-1 / 2 / 2.5, layers I, II, III) | `mkff-mp3` | 117–124 dB | Layer III CBR 8–320 kbit/s, joint stereo, Info / LAME gapless tag |
| AAC-LC (raw, ADTS, LATM / LOAS) | `mkff-aac` | 136–138 dB | AAC-LC 1–8 ch, TNS, M/S, PNS, exact bitrate |
| FLAC | `mkff-flac` | bit-exact | levels 0–8, 4–32 bit, ~5% smaller than FFmpeg's |
| ALAC | `mkff-alac` | bit-exact | 16 / 20 / 24 / 32-bit |
| AC-3 / E-AC-3 (incl. 7.1 dependent substreams, AHT, SPX) | `mkff-ac3` | 99–137 dB | — |
| Opus (SILK, CELT, hybrid, multistream) | `mkff-opus` | SILK bit-exact, CELT / hybrid 81–87 dB; RFC 8251 vectors range-exact | — |
| Vorbis | `mkff-vorbis` | 130 dB | — |
| PCM (int / float / A-law / µ-law) | `mkff-audio-format` | exact | WAV / AIFF / MP4 |

Known gaps: HE-AAC plays its AAC-LC core upsampled (no SBR / PS yet); E-AC-3
enhanced coupling is unsupported; Opus FEC is not used.

Containers (`mkff-audio-format`): WAV / RF64 / BW64, AIFF / AIFF-C, raw MPEG audio
(ID3, Xing / VBRI / LAME gapless), ADTS, FLAC, Ogg (Vorbis / Opus / FLAC), MP4 / M4A /
MOV audio tracks (fragmented too); writers for each.

```rust
use mkff_audio::{AudioFile, AudioWriter, CodecId, EncoderSettings};

// Decode: gapless, sample-exact seek, random access at any rate.
let mut f = AudioFile::open("song.m4a")?;
f.seek(44_100 * 30)?;
let chunk = f.next_chunk()?;
let window = f.read(0, 1024, 48_000)?;

// Encode: the container comes from the extension (.mp3 .m4a .aac .flac .wav).
let s = EncoderSettings::new(CodecId::MpegAudio, 44_100, 2).with_bitrate(192_000);
let mut w = AudioWriter::create("out.mp3", s)?;
w.write(&[&left, &right])?;
w.finish()?;
```

## Media Kit (`mkff-mediakit`)

A BeOS-style Media Kit, in-process:

| BeOS | MKFF |
|------|------|
| `BMediaRoster` + `media_server` | `MediaRoster`: add-ons, nodes, connections, codec selection |
| `BMediaAddOn` / `flavor_info` | `MediaAddOn` / `FlavorInfo`, plus where a flavor runs (`Acceleration`) |
| `BBufferProducer` / `BBufferConsumer` / `BMediaEventLooper` | `MediaNode`: one thread and timed event queue per node |
| `media_format` negotiation | `MediaFormat` wildcards: `FormatProposal` → `AcceptFormat` → `PrepareToConnect` → `Connect` |
| `BBuffer` / `BBufferGroup` / areas | `Buffer` / `BufferGroup` over an `Area` (named shared memory on Windows) |
| `BTimeSource` | `TimeSource`: performance ↔ real time, published by the master clock |
| graphics accelerants | `Accelerant`: one per GPU |

The accelerant keeps BeOS's driver model and maps it onto Windows:

| BeOS accelerant API | Windows implementation |
|---------------------|------------------------|
| `get_accelerant_hook(feature)` | DXVA decoder profiles, D3D11 video processor, DXGI overlay planes, Media Foundation hardware encoders |
| `acquire_engine` / `release_engine` | the multithread-protected D3D11 immediate context |
| sync tokens | D3D11 event queries |

Every GPU decode hook becomes a roster flavor (hardware output stays in D3D11
textures, or is read back to NV12 / P010). `mkff-hwenc` registers NVENC / Quick
Sync / AMF H.264 encoders and `mkff-audio` registers the software audio codecs, so
one call picks the best codec and falls back cleanly (`MKFF_ACCEL=auto|hw|sw`):

```rust
use mkff_mediakit::{AccelPolicy, CodecFamily, MediaFormat};

let roster = mkff_audio::addon::roster(); // GPU accelerants + MKFF audio codecs
let h264 = MediaFormat::encoded(CodecFamily::H264, avcc, 1920, 1080);
let (flavor, mut dec) = roster.video_decoder(&h264, AccelPolicy::Auto)?;
println!("{} via {}", flavor.info.name, flavor.info.accel);
```

`cargo run -p mkff-mediakit --example accel_info` lists each GPU's hooks. HEVC,
VP9 and AV1 decode hooks appear when Windows has the matching Media Foundation
decoders (the HEVC / VP9 / AV1 Video Extensions from the Microsoft Store).

## Software codecs (`mkff-codecs`)

Pure-Rust, safe-code demux and decode for every container and codec MKFF
handles, on every platform, with no native codec library. Audio is MKFF's own
[`mkff-audio`](#mkff-audio-mkff-audio): WAV / AIFF, MP3, ADTS, FLAC, Ogg and MP4 / MOV
audio never touch FilmCraft (`MediaFile::audio_is_native`). Video, and the audio of
Matroska / WebM, AVI, MXF and MPEG-TS / PS, still come from the
[FilmCraft](https://github.com/storytold/filmcraft/tree/main/crates) codec crates
(MIT OR Apache-2.0, written from the public specifications), pinned to one revision.

| Kind | Supported |
|------|-----------|
| Containers | MP4 / MOV / 3GP, Matroska / WebM, AVI (OpenDML), MXF (OP1a / OP-Atom), MPEG-TS / M2TS, MPEG-PS / VOB, Ogg, YUV4MPEG2, standalone audio files; IVF and Annex-B elementary streams via the packet decoders |
| Video | H.264 (8-bit 4:2:0), HEVC Main / Main10, VP8, VP9 (profiles 0-3), AV1, MPEG-1 / MPEG-2, ProRes 422 / 4444 (+alpha), DNxHD / DNxHR, APV, Motion JPEG, uncompressed |
| Audio | AAC (LC, HE-AAC core; raw, ADTS, LATM), Opus, AC-3 / E-AC-3, MPEG audio layers I / II / III, FLAC, ALAC, Vorbis, PCM / LPCM |

Checked against an independent decoder on `testdata/codecs/`: H.264, HEVC
Main10, VP8, VP9 and AV1 are bit-exact; ProRes, DNxHR and MPEG-2 within ±1
LSB (IDCT rounding the specs allow).

Surround AC-3 / E-AC-3 in MP4 / MOV keeps every channel: the sample entry's
`ChannelCount` is 2 by specification (ETSI TS 102 366 Annex F); MKFF's MP4 demuxer
takes the real layout from `dac3` / `dec3` (`testdata/codecs/tone51_*.mp4`).

**Rust:**

```toml
[dependencies]
mkff-codecs = { git = "https://github.com/futureboard/mediakit-foundation" }
# or, alongside libmkff:  mkff = { git = "...", features = ["software-codecs"] }  → mkff::codecs
```

```rust
use mkff_codecs::{MediaFile, VideoCodec, VideoDecoder};

// Any file: random access by frame, audio at any rate.
let clip = MediaFile::open("clip.mkv")?;
let frame = clip.video_frame(42)?;            // planar YUV 8/10/12-bit or RGBA
let nv12 = frame.to_nv12();                   // or to_p010() / to_rgba8()
let pcm = clip.audio(0, 48_000, 48_000)?;     // 1 s of stream 0 at 48 kHz

// Elementary streams, e.g. Annex-B AUs from mkff::Mp4Demux:
let mut dec = VideoDecoder::new(VideoCodec::H264)?;
for frame in dec.decode(access_unit, pts)? { /* … */ }
```

`cargo run -p mkff-codecs --release --example probe -- clip.mxf` prints what
it sees and can dump NV12 / native planes.

**Encode + mux (`mkff_codecs::encode`):** H.264 (Baseline / Main / High, CRF / CBR / VBR,
optional B-frames) and ProRes (Proxy … 4444 XQ, alpha) video from RGBA8 (or planar 4:2:0 for
H.264), AAC-LC audio (1–8 channels, CBR), written to MP4 / MOV by `MediaWriter`, either
progressive or fragmented (a fragment per video keyframe, so a recording cut short stays
playable). Video encoders and the MP4 writer come from the pinned FilmCraft revision
(`filmcraft-h264enc`, `filmcraft-prores`, `filmcraft-isobmff`); the AAC encoder is MKFF's own
`mkff-aac`. For MP3 / FLAC / ALAC / WAV output, use `mkff_audio::AudioWriter`.

```rust
use mkff_codecs::encode::{AudioEncoder, Container, MediaWriter, VideoEncoder, VideoEncoderConfig};

let mut video = VideoEncoder::new(VideoEncoderConfig::h264(1920, 1080, 60, 1))?;
let mut audio = AudioEncoder::aac(48_000, 2, 192_000)?;
let mut out = MediaWriter::create("show.mp4", Container::Mp4, /* fragmented */ true)?;
let (v, a) = (out.add_video_track(&video)?, out.add_audio_track(&audio)?);
for p in video.encode_rgba(&rgba, frame_index)? { out.write(v, &p)?; }
for p in audio.encode(&[&left, &right])? { out.write(a, &p)?; }
// … then video.flush() / audio.flush() into `out`, and out.finish()
```

`cargo test -p mkff-codecs --test encode` round-trips every encoder through `MediaFile`.

**Hardware H.264 encode (`mkff-hwenc`):** NVIDIA **NVENC**, Intel **Quick Sync (QSV)** and AMD
**AMF** through the Media Foundation encoder each driver installs (Windows; nothing linked at
build time). Encoders on every GPU are found, including ones without a display (`MFTEnum2` per
DXGI adapter), discrete GPUs first. RGBA or NV12 in, the same `Packet`s as `mkff_codecs::encode`
out (AVCC), plus the `avcC` for `MediaWriter::add_h264_track` or an FLV / RTMP muxer. CBR /
peak-constrained VBR / constant quality, GOP, profile, speed / quality, low latency, no B-frames.

```rust
use mkff_hwenc::{HwEncoderConfig, HwVideoEncoder, Vendor};

let mut enc = HwVideoEncoder::new(Some(Vendor::Nvidia), HwEncoderConfig::cbr(1920, 1080, 60, 1, 6000))?;
for p in enc.encode_rgba(&rgba, frame_index)? { /* … */ }
let mut out = MediaWriter::create("show.mp4", Container::Mp4, true)?;
let v = out.add_h264_track(enc.avcc().unwrap(), 1920, 1080, enc.timescale())?;
```

1080p60 on a GTX 1060 / UHD 730: ≈ 3 ms per frame including RGBA → NV12 (software: 11–23 ms).
`cargo test -p mkff-hwenc --release` encodes with every encoder present, muxes with `MediaWriter`
and decodes the result back through `MediaFile` (corners, sizes not a multiple of 16, CBR / VBR /
quality).

**C (`libmkff_swcodecs`):** build with `cargo build -p mkff-codecs-capi
--release` (→ `target/release/libmkff_swcodecs.{so,dylib,a}` /
`mkff_swcodecs.dll`), include [`mkff/codecs.h`](include/mkff/codecs.h). It is
independent of `libmkff`; link either or both.

```c
MKFF_SwMedia *m;
mkff_sw_media_open_path("clip.webm", &m);
MKFF_SwFrame *f;
mkff_sw_media_read_video_frame(m, 0, &f);
MKFF_SwFrameInfo fi; MKFF_INIT_STRUCT_HEADER(&fi);
mkff_sw_frame_get_info(f, &fi);               /* planes, depth, chroma, colour */
mkff_sw_frame_destroy(f);
mkff_sw_media_destroy(m);
```

Packet decoders (`mkff_sw_video_decoder_*`, `mkff_sw_audio_decoder_*`) follow
the same submit / receive / flush contract as `mkff_video_decoder_*`.
`MKFF_VideoCodec` gains `VP8`, `MPEG1`, `MPEG2`, `PRORES`, `DNXHD`, `APV` and
`MJPEG` (software-only; libmkff's hardware path rejects them as before).

## Codec coverage (this milestone)

| Codec | Profiles | Output | HW | SW |
|-------|----------|--------|----|----|
| H.264 | Baseline/Main (existing) | NV12 | Linux/Windows/macOS | — |
| HEVC | Main (8-bit 4:2:0), Main10 (10-bit 4:2:0) | NV12 / P010 | VA-API / D3D11 DXVA / VideoToolbox | libhevc Main→NV12 (optional) |

Everything else (and software H.264 / HEVC Main10) is in
[`mkff-codecs`](#software-codecs-mkff-codecs).

Software fallback uses Ittiam libhevc, which is **8-bit only** in the
pinned tree (`IHEVCD_UNSUPPORTED_BIT_DEPTH` for Main10). Main10 decode
and P010 output are hardware-backed (VA-API / D3D11 / VideoToolbox when
the device supports them). `SOFTWARE_ONLY` on a Main10 stream returns
`MKFF_RESULT_ERROR_CODEC_UNAVAILABLE`.

Rejected: 4:2:2 / 4:4:4, non-8/10 bit depth, tiles/WPP on HW paths this milestone.

## Vulkan import (`mkff-vk`)

Linux dma-buf → `VkImage` via `VK_EXT_image_drm_format_modifier`. NV12
today; P010 drm fourcc is exported when Main10 surfaces are available.

## MP4 demux

Progressive (non-fragmented) MP4/MOV with `moov` + `mdat`. First `vide`
track only; sample entries **`avc1`** (H.264) and **`hvc1`/`hev1`** (HEVC).
Samples are converted to Annex-B (length-prefixed NALs → start codes;
`avcC`/`hvcC` parameter sets prepended on the first / sync samples).

C API: `mkff_mp4_demux_open_path` / `open_memory`, `get_video_track`,
`read_access_unit`, `seek_sample`. Rust: `mkff::Mp4Demux`.

Not supported: audio tracks, fragmented MP4 (`moof`/`mdat` segments),
encryption (`encv`), edit-list reordering beyond basic `ctts`.

Fixtures: `testdata/tiny_baseline_64x64.mp4` (`avc1`),
`testdata/tiny_main_256x144.mp4` (`hvc1`).

## WGPU demos (`mkff-wgpu`)

Portable demos: decode → `map_cpu_planes` (NV12) → upload / egui.

**Fullscreen WGSL viewer** (R8 + RG8 upload → YUV→RGB; HEVC Annex-B SW):

```sh
cargo run -p mkff-wgpu --example view
cargo run -p mkff-wgpu --example view -- path/to/clip.hevc
```

**Simple player** (egui on wgpu — play/pause, frame step, loop, FPS). Plays
any container / codec: MP4 and Annex-B go to libmkff's hardware decoders
first; everything else, and anything libmkff can't decode on this machine,
falls back to `mkff-codecs`:

```sh
cargo run -p mkff-wgpu --example player
cargo run -p mkff-wgpu --example player -- testdata/tiny_main_256x144.mp4
cargo run -p mkff-wgpu --example player -- testdata/tiny_baseline_64x64.mp4
cargo run -p mkff-wgpu --example player -- path/to/clip.hevc
cargo run -p mkff-wgpu --example player -- testdata/codecs/vp9_opus.webm
cargo run -p mkff-wgpu --example player -- testdata/codecs/prores_hq.mov
```

Defaults to `testdata/tiny_main_256x144.mp4`. Progressive MP4 uses demux +
`AUTO` decode (H.264 HW on Windows when available; HEVC may use HW or
libhevc SW for Main). Annex-B `.hevc` still uses `SOFTWARE_ONLY` + libhevc.
Frames are decoded up front; no audio. Hardware zero-copy into wgpu is
not wired yet. Needs a GPU/display; not part of default CI.

## Command-line tool (`mkfx`)

`mkfx` (crate `mkff-cli`) is an FFmpeg-style command line over MKFF:
`mkfx [global] {[input options] -i input}... {[output options] output}...`. Input
options (`-ss`, `-t`, `-to` before `-i`) seek the input; output options shape each
output file.

```sh
mkfx -i song.flac -c:a mp3 -b:a 192k song.mp3              # MP3 encode (gapless Info / LAME tag)
mkfx -i talk.wav -ar 16000 -ac 1 -af volume=-3dB talk.m4a  # resample, downmix, gain, AAC
mkfx -ss 30 -i talk.m4a -t 60 excerpt.wav                  # sample-exact cut
mkfx -i movie.mp4 -c:a copy movie.m4a                      # audio stream copy (remux)
mkfx -i clip.mov -c:v h264_hw -b:v 6M -vf scale=1280:-2 clip.mp4   # GPU (NVENC / QSV / AMF)
mkfx -i clip.mp4 -c:v prores -profile:v hq -an clip.mov    # ProRes intermediate
mkfx -i anything.mkv                                       # describe the input
mkfx -codecs | -encoders | -formats | -hwaccels            # what this build / machine can do
```

| | |
|---|---|
| Audio encoders | `aac`, `mp3` (`libmp3lame` alias), `flac` (`-compression_level`), `alac`, `pcm_s16le` / `pcm_s24le` / `pcm_f32le` |
| Video encoders | `h264` (software; `-crf`, `-b:v`, `-preset`, `-g`, `-bf`), `h264_hw` / `h264_nvenc` / `h264_qsv` / `h264_amf` (GPU through `mkff-hwenc`), `prores` (`-profile:v proxy…4444xq`) |
| Outputs | `.mp3`, `.aac`, `.flac`, `.wav`, `.m4a`, `.mp4` / `.mov` (H.264 / ProRes + AAC), `.ogg` / `.opus` (Opus stream copy) |
| Stream copy | `-c:a copy` between MKFF's own containers (MP4 / M4A, MP3, ADTS, FLAC, Ogg Opus), gapless metadata kept |
| Other | `-map`, `-an` / `-vn`, `-ar`, `-ac` (standard downmix, normalised), `-af volume=`, `-r`, `-s` / `-vf scale=W:H` (`-1` / `-2` keep aspect), `-movflags +frag_keyframe`, `-y` / `-n`, `-v`, `-nostats` |

Not yet: video stream copy, Opus / Vorbis / AC-3 encoders, filters beyond `volume`
and `scale`. `mkfx -hwaccels` lists the Media Kit's accelerants (one per GPU) and
their decode / encode / video-processor hooks.

## MKFF Player (`mkff-player`)

A desktop media player for everything `mkff-codecs` decodes, in pure Rust
(no FFmpeg, no CMake): egui on wgpu (Vulkan / Metal / D3D12 / GL), audio
through cpal (ALSA / CoreAudio / WASAPI).

```sh
cargo run -p mkff-player --release -- path/to/clip.mp4
cargo run -p mkff-player --release -- --help
```

Tested with Dolby's DD+ test signal
`Audio_ID_720p_50fps_h264_514ch_448kbps_ddp.mp4` (H.264 720p50 + E-AC-3 5.1):
32 s in 32.0 s wall time, decode ≈ 1.2 ms / frame, A/V within one frame, all six
channels decoded and downmixed for stereo output.

- **Playback engine** (`mkff_player`, no UI dependency): a video decode thread
  (bounded queue, skips ahead when decoding falls behind), an audio decode
  thread feeding a lock-free ring buffer, and an **audio-master clock** from the
  device callback's frame counter and reported latency (system clock without
  audio). Seeks start an *epoch*: the callback drops stale samples without
  blocking, the clock waits for the first picture (preroll) and one audio chunk.
  Decoder panics and errors are caught and reported; a dead audio device falls
  back to the system clock.
- **Video**: Y'CbCr planes uploaded as textures and converted in a WGSL shader
  (BT.601 / 709 / 2020 matrices, limited / full range, any chroma subsampling,
  sRGB or linear targets); pixel aspect respected. 10 / 12-bit is shown at 8 bits.
- **Audio**: resampled to the device rate; 5.1 / 7.1 downmixed to stereo
  (ITU-R BS.775, LFE dropped) or played discretely on a surround device;
  click-free volume ramps.
- **UI**: auto-hiding controls, seek bar with time preview and scrubbing, volume /
  mute, speed 0.25–2× (resampled: pitch follows), loop, frame stepping, full
  screen, PNG snapshot, statistics overlay (I), drag & drop, recent files,
  remembered volume / window, keyboard shortcuts (H lists them).
- **Headless**: `mkff-player --audio null --headless 30 clip.mp4` plays without a
  window on a simulated audio device and prints frame / drop / A-V statistics;
  `cargo test -p mkff-player` runs the engine against `testdata/` the same way.

Linux builds need `libasound2-dev`; the file dialog uses xdg-desktop-portal or
zenity (without either, drop files onto the window or pass them on the command
line). Not yet: libmkff hardware decode / zero-copy upload, Dolby Atmos (JOC)
objects (the 5.1 bed plays), HDR tone mapping, subtitles, playlists.

## Non-goals (this milestone)

Fragmented MP4 in libmkff's own demuxer, bitstream seeking as a product
feature, hardware VP9/AV1, mid-stream resolution change,
long-term reference picture edge cases beyond the supported Main/Main10
subset.

## License

MIT — see [LICENSE](LICENSE). Third-party libhevc is Apache-2.0; the
FilmCraft codec crates used by `mkff-codecs` are MIT OR Apache-2.0 (used
under MIT), and their audio bootstrap decoders (still compiled in for the
containers MKFF does not demux itself yet) come from Symphonia (MPL-2.0,
unmodified). `mkff-opus` ports the RFC 6716 reference decoder (BSD-3-Clause).
See [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES).
