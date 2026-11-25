//! The link from Futureboard Studio to the native visualizer app
//! (`apps/native/visualizer`): one named shared-memory region Studio writes
//! and the app reads.
//!
//! # Region (protocol version 1)
//!
//! | Offset | Size | What |
//! | --- | --- | --- |
//! | 0 | 128 | [`Header`]: magic, version, layout, heartbeats, the app's track request |
//! | 128 | 256 | [`Frame`]: transport, master and monitor levels, track count |
//! | 384 | 512 × 128 | [`TrackEntry`] table, the first `Frame::track_count` in mixer order |
//! | 65 920 | 64 + 2 × 4 × 65 536 | master ring: post-fader master, planar stereo `f32` |
//! | 590 272 | 64 + 2 × 4 × 65 536 | track ring: the requested track's post-fader audio |
//!
//! Everything is little-endian, at these fixed offsets (the header repeats
//! them). The C++ side mirrors it in `apps/native/visualizer/src/link/FutureboardLink.hpp`.
//!
//! - The frame and the track table are written under the header's seqlock
//!   `seq`: odd while writing. A reader copies them, then accepts the copy
//!   only if `seq` was even and unchanged.
//! - A ring's first 8 bytes are its write head (frames written since the
//!   region was created). The writer fills the slots, then publishes the head;
//!   a reader keeps its own cursor and skips forward when it falls more than
//!   a ring behind.
//! - The reader stamps `reader_heartbeat_ms`. Studio does no work for the
//!   link — no audio copies, no table — unless a reader stamped it in the
//!   last [`READER_TIMEOUT_MS`]. Studio stamps `writer_heartbeat_ms`, so the
//!   app can say when Studio is gone.
//! - The reader asks for one track's audio by writing its key into
//!   `requested_track_key`; `tapped_track_key` says which track the track
//!   ring is carrying.

mod mapping;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
use std::time::{SystemTime, UNIX_EPOCH};

use mapping::Mapping;

/// `"FBVZ"` read as a little-endian `u32`.
pub const MAGIC: u32 = u32::from_le_bytes(*b"FBVZ");
pub const VERSION: u32 = 1;
pub const MAX_TRACKS: usize = 512;
pub const NAME_BYTES: usize = 48;
pub const PROJECT_NAME_BYTES: usize = 64;
/// Frames in each audio ring (a power of two).
pub const AUDIO_FRAMES: usize = 1 << 16;
/// A reader whose heartbeat is older than this is gone.
pub const READER_TIMEOUT_MS: u64 = 2_000;
/// A writer whose heartbeat is older than this is gone: a new Studio takes
/// the region over, and a reader reopens it (a POSIX writer unlinks it on
/// exit, so the next Studio's region is a new one).
pub const WRITER_TIMEOUT_MS: u64 = 3_000;

/// The region's name. On Windows it lives in the session namespace, so a
/// visualizer started by the same user finds it.
#[cfg(windows)]
pub const REGION_NAME: &str = "Local\\FutureboardVisualizer.1";
#[cfg(not(windows))]
pub const REGION_NAME: &str = "/fb_visualizer_1";

/// [`TrackEntry::kind`].
pub mod kind {
    pub const AUDIO: u8 = 0;
    pub const MIDI: u8 = 1;
    pub const INSTRUMENT: u8 = 2;
    pub const BUS: u8 = 3;
    pub const RETURN: u8 = 4;
    pub const GROUP: u8 = 5;
    pub const VIDEO: u8 = 6;
    /// One output pair of a multi-output instrument.
    pub const PLUGIN_OUTPUT: u8 = 7;
}

/// [`TrackEntry::flags`].
pub mod flags {
    pub const MUTED: u8 = 1;
    pub const SOLO: u8 = 2;
    pub const ARMED: u8 = 4;
    pub const SELECTED: u8 = 8;
    pub const CLIPPED: u8 = 16;
}

/// [`Frame::transport`].
pub mod transport {
    pub const PLAYING: u32 = 1;
    pub const RECORDING: u32 = 2;
    pub const LOOPING: u32 = 4;
}

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub total_size: u32,
    pub header_size: u32,
    pub max_tracks: u32,
    pub track_entry_size: u32,
    pub audio_frames: u32,
    /// Rate of the audio in the rings; 0 before any.
    pub sample_rate: AtomicU32,
    pub writer_pid: u32,
    pub frame_offset: u32,
    pub tracks_offset: u32,
    pub master_ring_offset: u32,
    pub track_ring_offset: u32,
    pub _reserved0: u32,
    /// Milliseconds since the Unix epoch, stamped by Studio about twice a
    /// second.
    pub writer_heartbeat_ms: AtomicU64,
    /// Stamped by the app while it reads.
    pub reader_heartbeat_ms: AtomicU64,
    /// The track whose audio the app wants ([`track_key`]); 0 for none.
    pub requested_track_key: AtomicU64,
    /// The track the track ring carries now; 0 for none.
    pub tapped_track_key: AtomicU64,
    /// Seqlock over the frame and the track table.
    pub seq: AtomicU32,
    pub _reserved1: [u32; 9],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Levels {
    pub peak_l: f32,
    pub peak_r: f32,
    pub rms_l: f32,
    pub rms_r: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// Bumped every write.
    pub counter: u64,
    pub position_beats: f64,
    pub position_seconds: f64,
    pub bpm: f32,
    pub time_sig_num: u32,
    pub time_sig_den: u32,
    /// [`transport`] bits.
    pub transport: u32,
    /// Post-fader master, linear; peaks are the highest since the last frame.
    pub master: Levels,
    /// Control Room output.
    pub monitor: Levels,
    pub master_volume_db: f32,
    pub track_count: u32,
    /// The track selected in Studio ([`track_key`]); 0 for none.
    pub selected_track_key: u64,
    /// UTF-8, NUL-padded.
    pub project_name: [u8; PROJECT_NAME_BYTES],
    pub _reserved: [u8; 104],
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            counter: 0,
            position_beats: 0.0,
            position_seconds: 0.0,
            bpm: 0.0,
            time_sig_num: 4,
            time_sig_den: 4,
            transport: 0,
            master: Levels::default(),
            monitor: Levels::default(),
            master_volume_db: 0.0,
            track_count: 0,
            selected_track_key: 0,
            project_name: [0; PROJECT_NAME_BYTES],
            _reserved: [0; 104],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrackEntry {
    /// [`track_key`] of the track's id: stable while the track exists.
    pub key: u64,
    /// [`kind`].
    pub kind: u8,
    /// [`flags`] bits.
    pub flags: u8,
    /// Folder depth, 0 at the top.
    pub depth: u8,
    pub _pad: u8,
    /// `0xRRGGBB`.
    pub color: u32,
    pub volume_db: f32,
    /// −1 (left) … 1 (right).
    pub pan: f32,
    /// Post-fader, linear; peaks are the highest since the last frame.
    pub levels: Levels,
    /// UTF-8, NUL-padded, cut at a character boundary.
    pub name: [u8; NAME_BYTES],
    pub _reserved: [u8; 40],
}

impl Default for TrackEntry {
    fn default() -> Self {
        Self {
            key: 0,
            kind: 0,
            flags: 0,
            depth: 0,
            _pad: 0,
            color: 0,
            volume_db: 0.0,
            pan: 0.0,
            levels: Levels::default(),
            name: [0; NAME_BYTES],
            _reserved: [0; 40],
        }
    }
}

pub const HEADER_SIZE: usize = 128;
pub const FRAME_OFFSET: usize = HEADER_SIZE;
pub const TRACKS_OFFSET: usize = FRAME_OFFSET + std::mem::size_of::<Frame>();
pub const TRACK_ENTRY_SIZE: usize = std::mem::size_of::<TrackEntry>();
pub const RING_HEAD_SIZE: usize = 64;
pub const RING_SIZE: usize = RING_HEAD_SIZE + 2 * 4 * AUDIO_FRAMES;
pub const MASTER_RING_OFFSET: usize = TRACKS_OFFSET + MAX_TRACKS * TRACK_ENTRY_SIZE;
pub const TRACK_RING_OFFSET: usize = MASTER_RING_OFFSET + RING_SIZE;
pub const TOTAL_SIZE: usize = TRACK_RING_OFFSET + RING_SIZE;

const _: () = assert!(std::mem::size_of::<Header>() == HEADER_SIZE);
const _: () = assert!(std::mem::size_of::<Frame>() == 256);
const _: () = assert!(TRACK_ENTRY_SIZE == 128);
const _: () = assert!(MASTER_RING_OFFSET == 65_920);
const _: () = assert!(TRACK_RING_OFFSET == 590_272);
const _: () =
    assert!(MASTER_RING_OFFSET.is_multiple_of(64) && TRACK_RING_OFFSET.is_multiple_of(64));
const _: () = assert!(AUDIO_FRAMES.is_power_of_two());

/// FNV-1a of a track id, never 0 (0 means "no track").
pub fn track_key(id: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    if hash == 0 { 1 } else { hash }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `text` into a NUL-padded buffer, cut at a UTF-8 character boundary and
/// always leaving a NUL.
pub fn fill_name<const N: usize>(out: &mut [u8; N], text: &str) {
    *out = [0; N];
    let mut end = text.len().min(N - 1);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    out[..end].copy_from_slice(&text.as_bytes()[..end]);
}

/// The text of a NUL-padded name.
pub fn name_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// One stereo ring inside the region.
struct Ring {
    base: *mut u8,
}

impl Ring {
    fn head(&self) -> &AtomicU64 {
        // SAFETY: the ring's first 8 bytes, 64-byte aligned in the region.
        unsafe { &*(self.base as *const AtomicU64) }
    }

    fn channel(&self, right: bool) -> *mut f32 {
        let offset = RING_HEAD_SIZE + if right { 4 * AUDIO_FRAMES } else { 0 };
        // SAFETY: inside the ring.
        unsafe { self.base.add(offset).cast() }
    }

    fn write(&self, left: &[f32], right: &[f32]) {
        let frames = left.len().min(right.len());
        let head = self.head().load(Ordering::Relaxed);
        let (l, r) = (self.channel(false), self.channel(true));
        for i in 0..frames {
            let slot = ((head + i as u64) as usize) & (AUDIO_FRAMES - 1);
            // SAFETY: `slot` is inside the ring.
            unsafe {
                l.add(slot).write_volatile(left[i]);
                r.add(slot).write_volatile(right[i]);
            }
        }
        self.head().store(head + frames as u64, Ordering::Release);
    }

    /// Copies the frames since `cursor` (or, when it fell behind, the newest
    /// ones that are safe to read). Returns `(frames, next cursor, skipped)`.
    fn read(&self, cursor: u64, left: &mut [f32], right: &mut [f32]) -> (usize, u64, u64) {
        let head = self.head().load(Ordering::Acquire);
        let margin = 2_048u64;
        let oldest = head.saturating_sub(AUDIO_FRAMES as u64 - margin);
        let (start, skipped) = if cursor < oldest {
            (oldest, oldest - cursor)
        } else {
            (cursor.min(head), 0)
        };
        let frames = ((head - start) as usize).min(left.len()).min(right.len());
        let (l, r) = (self.channel(false), self.channel(true));
        for i in 0..frames {
            let slot = ((start + i as u64) as usize) & (AUDIO_FRAMES - 1);
            // SAFETY: inside the ring.
            unsafe {
                left[i] = l.add(slot).read_volatile();
                right[i] = r.add(slot).read_volatile();
            }
        }
        (frames, start + frames as u64, skipped)
    }
}

/// The region, as one process maps it.
struct Region {
    mapping: Mapping,
}

impl Region {
    fn header(&self) -> &Header {
        // SAFETY: offset 0, `HEADER_SIZE` bytes, aligned by the mapping.
        unsafe { &*(self.mapping.ptr() as *const Header) }
    }

    fn frame_ptr(&self) -> *mut Frame {
        // SAFETY: inside the region.
        unsafe { self.mapping.ptr().add(FRAME_OFFSET).cast() }
    }

    fn track_ptr(&self, index: usize) -> *mut TrackEntry {
        // SAFETY: `index < MAX_TRACKS` at every caller.
        unsafe {
            self.mapping
                .ptr()
                .add(TRACKS_OFFSET + index * TRACK_ENTRY_SIZE)
                .cast()
        }
    }

    fn reader_alive(&self) -> bool {
        let seen = self.header().reader_heartbeat_ms.load(Ordering::Acquire);
        seen != 0 && now_ms().saturating_sub(seen) < READER_TIMEOUT_MS
    }

    fn ring(&self, offset: usize) -> Ring {
        // SAFETY: inside the region.
        Ring {
            base: unsafe { self.mapping.ptr().add(offset) },
        }
    }
}

/// Studio's side: creates the region and writes the frame and track table.
/// [`Writer::audio`] hands out the part that writes the audio rings, for a
/// thread of its own.
pub struct Writer {
    region: Arc<Region>,
    counter: u64,
}

/// Writes the audio rings and reads the reader's state; shareable with the
/// thread that pumps audio while [`Writer`] stays on the UI thread.
#[derive(Clone)]
pub struct AudioWriter {
    region: Arc<Region>,
}

impl Writer {
    /// Creates the region at [`REGION_NAME`]. Fails when another Studio is
    /// already publishing; takes over a region whose writer stopped stamping
    /// its heartbeat (a visualizer still holding it open after Studio quit,
    /// or a Studio that crashed).
    pub fn create() -> std::io::Result<Self> {
        Self::create_named(REGION_NAME)
    }

    /// Creates the region under `name` (tests, side-by-side tools).
    pub fn create_named(name: &str) -> std::io::Result<Self> {
        let (mapping, existed) = Mapping::create_or_open(name, TOTAL_SIZE)?;
        debug_assert!(mapping.size() >= TOTAL_SIZE);
        let region = Region { mapping };
        if existed {
            return Self::take_over(region);
        }
        // SAFETY: fresh, zero-filled, and nobody reads it before the magic.
        unsafe {
            let header = region.mapping.ptr() as *mut Header;
            (*header).version = VERSION;
            (*header).total_size = TOTAL_SIZE as u32;
            (*header).header_size = HEADER_SIZE as u32;
            (*header).max_tracks = MAX_TRACKS as u32;
            (*header).track_entry_size = TRACK_ENTRY_SIZE as u32;
            (*header).audio_frames = AUDIO_FRAMES as u32;
            (*header).writer_pid = std::process::id();
            (*header).frame_offset = FRAME_OFFSET as u32;
            (*header).tracks_offset = TRACKS_OFFSET as u32;
            (*header).master_ring_offset = MASTER_RING_OFFSET as u32;
            (*header).track_ring_offset = TRACK_RING_OFFSET as u32;
            region.frame_ptr().write_volatile(Frame::default());
            fence(Ordering::Release);
            (*header).magic = MAGIC;
        }
        region
            .header()
            .writer_heartbeat_ms
            .store(now_ms(), Ordering::Release);
        Ok(Self {
            region: Arc::new(region),
            counter: 0,
        })
    }

    fn take_over(region: Region) -> std::io::Result<Self> {
        let header = region.header();
        let compatible = header.magic == MAGIC
            && header.version == VERSION
            && header.total_size as usize == TOTAL_SIZE;
        if !compatible {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "another program holds the visualizer link name",
            ));
        }
        let seen = header.writer_heartbeat_ms.load(Ordering::Acquire);
        if now_ms().saturating_sub(seen) < WRITER_TIMEOUT_MS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "the visualizer link is already published by another process",
            ));
        }
        // A writer that died mid-frame left the seqlock odd.
        let seq = header.seq.load(Ordering::Acquire);
        if !seq.is_multiple_of(2) {
            header.seq.store(seq.wrapping_add(1), Ordering::Release);
        }
        header.tapped_track_key.store(0, Ordering::Release);
        // SAFETY: inside the header; readers only display the pid.
        unsafe {
            std::ptr::addr_of_mut!((*(region.mapping.ptr() as *mut Header)).writer_pid)
                .write_volatile(std::process::id());
        }
        header
            .writer_heartbeat_ms
            .store(now_ms(), Ordering::Release);
        Ok(Self {
            region: Arc::new(region),
            counter: 0,
        })
    }

    /// Whether a reader stamped its heartbeat recently: the only time the
    /// writer has anything to do.
    pub fn reader_alive(&self) -> bool {
        self.region.reader_alive()
    }

    pub fn audio(&self) -> AudioWriter {
        AudioWriter {
            region: self.region.clone(),
        }
    }

    pub fn stamp_heartbeat(&self) {
        self.region
            .header()
            .writer_heartbeat_ms
            .store(now_ms(), Ordering::Release);
    }

    /// The track whose audio the reader asked for; 0 for none.
    pub fn requested_track_key(&self) -> u64 {
        self.region
            .header()
            .requested_track_key
            .load(Ordering::Acquire)
    }

    /// Says which track the track ring now carries.
    pub fn set_tapped_track_key(&self, key: u64) {
        self.region
            .header()
            .tapped_track_key
            .store(key, Ordering::Release);
    }

    /// Publishes a frame and the track table, under the seqlock. `tracks`
    /// past [`MAX_TRACKS`] are left out.
    pub fn write_frame(&mut self, frame: &Frame, tracks: &[TrackEntry]) {
        let count = tracks.len().min(MAX_TRACKS);
        self.counter = self.counter.wrapping_add(1);
        let mut frame = *frame;
        frame.counter = self.counter;
        frame.track_count = count as u32;
        let seq = &self.region.header().seq;
        let start = seq.load(Ordering::Relaxed);
        seq.store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: inside the region; readers validate through `seq`.
        unsafe {
            self.region.frame_ptr().write_volatile(frame);
            for (index, track) in tracks.iter().take(count).enumerate() {
                self.region.track_ptr(index).write_volatile(*track);
            }
        }
        seq.store(start.wrapping_add(2), Ordering::Release);
    }
}

impl AudioWriter {
    pub fn reader_alive(&self) -> bool {
        self.region.reader_alive()
    }

    pub fn set_sample_rate(&self, rate: u32) {
        let header = self.region.header();
        if header.sample_rate.load(Ordering::Relaxed) != rate {
            header.sample_rate.store(rate, Ordering::Release);
        }
    }

    /// Appends post-fader master audio. One thread writes each ring.
    pub fn write_master_audio(&self, left: &[f32], right: &[f32]) {
        self.region.ring(MASTER_RING_OFFSET).write(left, right);
    }

    /// Appends the tapped track's audio. One thread writes each ring.
    pub fn write_track_audio(&self, left: &[f32], right: &[f32]) {
        self.region.ring(TRACK_RING_OFFSET).write(left, right);
    }
}

/// A consistent copy of the frame and track table.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub frame: Frame,
    pub tracks: Vec<TrackEntry>,
}

/// The app's side, in Rust: what the C++ reader does, for tests and tools.
pub struct Reader {
    region: Region,
    master_cursor: u64,
    track_cursor: u64,
}

impl Reader {
    pub fn open() -> std::io::Result<Self> {
        Self::open_named(REGION_NAME)
    }

    pub fn open_named(name: &str) -> std::io::Result<Self> {
        let region = Region {
            mapping: Mapping::open(name, TOTAL_SIZE)?,
        };
        let header = region.header();
        if header.magic != MAGIC || header.version != VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a version 1 visualizer link",
            ));
        }
        let master_cursor = region
            .ring(MASTER_RING_OFFSET)
            .head()
            .load(Ordering::Acquire);
        let track_cursor = region
            .ring(TRACK_RING_OFFSET)
            .head()
            .load(Ordering::Acquire);
        Ok(Self {
            region,
            master_cursor,
            track_cursor,
        })
    }

    pub fn stamp_heartbeat(&self) {
        self.region
            .header()
            .reader_heartbeat_ms
            .store(now_ms(), Ordering::Release);
    }

    pub fn request_track(&self, key: u64) {
        self.region
            .header()
            .requested_track_key
            .store(key, Ordering::Release);
    }

    pub fn tapped_track_key(&self) -> u64 {
        self.region
            .header()
            .tapped_track_key
            .load(Ordering::Acquire)
    }

    pub fn sample_rate(&self) -> u32 {
        self.region.header().sample_rate.load(Ordering::Acquire)
    }

    pub fn writer_heartbeat_ms(&self) -> u64 {
        self.region
            .header()
            .writer_heartbeat_ms
            .load(Ordering::Acquire)
    }

    /// A consistent frame and track table, or `None` after a few tries that
    /// each raced a write.
    pub fn snapshot(&self) -> Option<Snapshot> {
        let seq = &self.region.header().seq;
        for _ in 0..64 {
            let before = seq.load(Ordering::Acquire);
            if !before.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: inside the region; validated through `seq` below.
            let frame = unsafe { self.region.frame_ptr().read_volatile() };
            let count = (frame.track_count as usize).min(MAX_TRACKS);
            let tracks: Vec<TrackEntry> = (0..count)
                .map(|index| unsafe { self.region.track_ptr(index).read_volatile() })
                .collect();
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == before {
                return Some(Snapshot { frame, tracks });
            }
        }
        None
    }

    /// Master audio since the last call: `(frames, skipped)`.
    pub fn read_master(&mut self, left: &mut [f32], right: &mut [f32]) -> (usize, u64) {
        let (frames, next, skipped) =
            self.region
                .ring(MASTER_RING_OFFSET)
                .read(self.master_cursor, left, right);
        self.master_cursor = next;
        (frames, skipped)
    }

    /// Track audio since the last call: `(frames, skipped)`.
    pub fn read_track(&mut self, left: &mut [f32], right: &mut [f32]) -> (usize, u64) {
        let (frames, next, skipped) =
            self.region
                .ring(TRACK_RING_OFFSET)
                .read(self.track_cursor, left, right);
        self.track_cursor = next;
        (frames, skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_name(tag: &str) -> String {
        #[cfg(windows)]
        let prefix = "Local\\FutureboardVisualizerTest";
        #[cfg(not(windows))]
        let prefix = "/fb_vz_test";
        format!("{prefix}_{tag}_{}", std::process::id())
    }

    #[test]
    fn the_layout_is_the_documented_one() {
        assert_eq!(MAGIC.to_le_bytes(), *b"FBVZ");
        assert_eq!(TRACKS_OFFSET, 384);
        assert_eq!(TOTAL_SIZE, 590_272 + 64 + 8 * 65_536);
        assert_eq!(std::mem::offset_of!(Header, writer_heartbeat_ms), 56);
        assert_eq!(std::mem::offset_of!(Header, requested_track_key), 72);
        assert_eq!(std::mem::offset_of!(Header, seq), 88);
        assert_eq!(std::mem::offset_of!(Frame, master), 40);
        assert_eq!(std::mem::offset_of!(Frame, track_count), 76);
        assert_eq!(std::mem::offset_of!(Frame, project_name), 88);
        assert_eq!(std::mem::offset_of!(TrackEntry, levels), 24);
        assert_eq!(std::mem::offset_of!(TrackEntry, name), 40);
    }

    #[test]
    fn keys_are_stable_and_never_zero() {
        assert_eq!(track_key("track-1"), track_key("track-1"));
        assert_ne!(track_key("track-1"), track_key("track-2"));
        assert_ne!(track_key(""), 0);
        // FNV-1a's published value for "a".
        assert_eq!(track_key("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn names_are_cut_on_a_character_boundary() {
        let mut name = [0u8; 8];
        fill_name(&mut name, "กีตาร์");
        let text = name_text(&name);
        assert!("กีตาร์".starts_with(&text) && !text.is_empty());
        fill_name(&mut name, "Kick");
        assert_eq!(name_text(&name), "Kick");
    }

    #[test]
    fn a_reader_sees_what_the_writer_wrote() {
        let name = unique_name("frame");
        let mut writer = Writer::create_named(&name).expect("create");
        assert!(
            Writer::create_named(&name).is_err(),
            "a second writer is refused"
        );
        let reader = Reader::open_named(&name).expect("open");
        assert!(!writer.reader_alive());
        reader.stamp_heartbeat();
        assert!(writer.reader_alive());

        let mut frame = Frame {
            bpm: 128.0,
            transport: transport::PLAYING,
            position_beats: 17.5,
            master: Levels {
                peak_l: 0.5,
                peak_r: 0.25,
                rms_l: 0.1,
                rms_r: 0.05,
            },
            ..Frame::default()
        };
        fill_name(&mut frame.project_name, "Demo Song");
        let tracks: Vec<TrackEntry> = (0..3)
            .map(|i| {
                let mut entry = TrackEntry {
                    key: track_key(&format!("track-{i}")),
                    kind: kind::AUDIO,
                    flags: if i == 1 { flags::SOLO } else { 0 },
                    color: 0x33_66_99,
                    volume_db: -3.0 * i as f32,
                    levels: Levels {
                        peak_l: 0.1 * i as f32,
                        ..Levels::default()
                    },
                    ..TrackEntry::default()
                };
                fill_name(&mut entry.name, &format!("Track {i}"));
                entry
            })
            .collect();
        writer.write_frame(&frame, &tracks);
        let snapshot = reader.snapshot().expect("consistent");
        frame.counter = 1;
        frame.track_count = 3;
        assert_eq!(snapshot.frame, frame);
        assert_eq!(snapshot.tracks, tracks);
        assert_eq!(name_text(&snapshot.frame.project_name), "Demo Song");

        reader.request_track(tracks[2].key);
        assert_eq!(writer.requested_track_key(), tracks[2].key);
        writer.set_tapped_track_key(tracks[2].key);
        assert_eq!(reader.tapped_track_key(), tracks[2].key);
    }

    #[test]
    fn a_writer_that_stopped_is_taken_over() {
        let name = unique_name("takeover");
        let first = Writer::create_named(&name).expect("create");
        let reader = Reader::open_named(&name).expect("open");
        first
            .region
            .header()
            .writer_heartbeat_ms
            .store(now_ms() - WRITER_TIMEOUT_MS - 1, Ordering::Release);
        first.region.header().seq.store(7, Ordering::Release);
        let mut second = Writer::create_named(&name).expect("take over");
        assert!(reader.writer_heartbeat_ms() + WRITER_TIMEOUT_MS > now_ms());
        second.write_frame(&Frame::default(), &[]);
        assert!(reader.snapshot().is_some(), "the seqlock is even again");
        assert!(
            Writer::create_named(&name).is_err(),
            "a live writer is not taken over"
        );
    }

    #[test]
    fn audio_arrives_in_order_and_a_late_reader_skips_ahead() {
        let name = unique_name("audio");
        let writer = Writer::create_named(&name).expect("create");
        let mut reader = Reader::open_named(&name).expect("open");
        let writer = writer.audio();
        writer.set_sample_rate(48_000);
        let left: Vec<f32> = (0..1_000).map(|i| i as f32).collect();
        let right: Vec<f32> = left.iter().map(|v| -v).collect();
        writer.write_master_audio(&left, &right);
        let (mut l, mut r) = (vec![0.0; 4_096], vec![0.0; 4_096]);
        let (frames, skipped) = reader.read_master(&mut l, &mut r);
        assert_eq!((frames, skipped), (1_000, 0));
        assert_eq!(&l[..1_000], &left[..]);
        assert_eq!(&r[..1_000], &right[..]);
        assert_eq!(reader.read_master(&mut l, &mut r).0, 0);
        assert_eq!(reader.sample_rate(), 48_000);

        // Far more than a ring: the reader resumes near the head.
        let block = vec![0.5f32; 8_192];
        for _ in 0..20 {
            writer.write_track_audio(&block, &block);
        }
        let (mut l, mut r) = (vec![0.0; AUDIO_FRAMES], vec![0.0; AUDIO_FRAMES]);
        let (frames, skipped) = reader.read_track(&mut l, &mut r);
        assert!(skipped > 0);
        assert!(frames <= AUDIO_FRAMES - 2_048);
        assert!(l[..frames].iter().all(|v| *v == 0.5));
    }

    #[test]
    fn a_reader_racing_writes_never_sees_a_torn_frame() {
        let name = unique_name("race");
        let mut writer = Writer::create_named(&name).expect("create");
        let reader = Reader::open_named(&name).expect("open");
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reading = {
            let done = done.clone();
            std::thread::spawn(move || {
                let mut seen = 0;
                while !done.load(Ordering::Relaxed) {
                    if let Some(snapshot) = reader.snapshot() {
                        // Every field of one write carries the same number.
                        let n = snapshot.frame.bpm;
                        assert_eq!(snapshot.frame.master.peak_l, n);
                        for track in &snapshot.tracks {
                            assert_eq!(track.volume_db, n);
                        }
                        seen += 1;
                    }
                }
                seen
            })
        };
        for i in 0..20_000u32 {
            let n = i as f32;
            let frame = Frame {
                bpm: n,
                master: Levels {
                    peak_l: n,
                    ..Levels::default()
                },
                ..Frame::default()
            };
            let tracks = vec![
                TrackEntry {
                    volume_db: n,
                    ..TrackEntry::default()
                };
                64
            ];
            writer.write_frame(&frame, &tracks);
        }
        done.store(true, Ordering::Relaxed);
        assert!(reading.join().unwrap() > 0);
    }
}
