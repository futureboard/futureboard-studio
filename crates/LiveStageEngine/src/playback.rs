//! Playback of a recorded take, for virtual soundcheck: each file of the
//! take feeds the channel it was recorded from, in place of the interface
//! input.
//!
//! ```txt
//! take folder ──► reader thread (symphonia: decode, seek, loop)
//!                   └─► one SPSC ring per file (≥ 4 s, made when the take loads)
//!                         └─► audio thread: one block per file, popped
//!                               └─► the assigned channel's input, while
//!                                   virtual soundcheck is on (see `graph`)
//! ```
//!
//! The audio thread only pops: a ring that runs dry is silence and a counted
//! underrun, never a wait. Locate and stop move a generation counter; until
//! the reader has refilled the rings from the new place the audio thread
//! plays silence for the stale generation:
//!
//! ```txt
//! control  locate_frame ← f; generation ← g
//! reader   sees g: stops pushing; flush_request ← g; waits for flushed = g
//! audio    sees flush_request ≠ flushed: empties every ring; flushed ← g
//! reader   seeks every file to f, refills, ready ← g
//! audio    pops only while ready = generation
//! ```

use std::cell::UnsafeCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::graph::{BYPASS_FADE_SECONDS, MAX_BLOCK};
use crate::ring::SampleRing;
use crate::session::{ChannelStrip, PlaybackTrack};

/// Seconds of audio each file's ring holds ahead of the audio thread.
pub const RING_SECONDS: usize = 4;

/// How much the reader decodes at a time, frames.
const CHUNK_FRAMES: usize = 4096;

/// How much must be buffered after a locate before it plays.
const PREFILL_SECONDS: f64 = 0.25;

/// The files a take is made of: what the recorder writes.
const AUDIO_EXTENSIONS: [&str; 2] = ["wav", "flac"];

/// One file of a take, as read from its header.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TakeFile {
    pub name: String,
    #[serde(skip)]
    pub path: PathBuf,
    pub channels: u16,
    pub rate: u32,
    pub seconds: f64,
    #[serde(skip)]
    pub frames: u64,
}

/// A take folder: its files, by name.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TakeInfo {
    pub name: String,
    pub path: PathBuf,
    pub files: Vec<TakeFile>,
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| e.eq_ignore_ascii_case(a)))
}

/// A FLAC file read with its STREAMINFO's minimum block size corrected.
///
/// `flacenc` (what the recorder's FLAC encoder uses) writes the last
/// block's length as the minimum block size. symphonia then takes the
/// fixed-blocksize stream for a variable one and accepts none of its frames.
/// When the first frame says the stream is fixed-blocksize, the minimum is
/// read as the maximum, as the FLAC format has it.
struct FlacFile {
    file: std::fs::File,
    len: u64,
    pos: u64,
    /// Bytes 8..10 (STREAMINFO's minimum block size) as read instead.
    min_block: Option<[u8; 2]>,
}

impl FlacFile {
    const MIN_BLOCK_AT: u64 = 8;

    fn open(path: &Path) -> std::io::Result<Self> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let mut min_block = None;
        let mut head = [0u8; 8 + 34];
        if file.read_exact(&mut head).is_ok() && &head[..4] == b"fLaC" && head[4] & 0x7f == 0 {
            // Past the metadata blocks to the first frame's sync code.
            let mut at = 4u64;
            let mut header = [0u8; 4];
            loop {
                file.seek(SeekFrom::Start(at))?;
                file.read_exact(&mut header)?;
                let length = u64::from(u32::from_be_bytes([0, header[1], header[2], header[3]]));
                at += 4 + length;
                if header[0] & 0x80 != 0 {
                    break;
                }
            }
            let mut sync = [0u8; 2];
            file.seek(SeekFrom::Start(at))?;
            let fixed = file.read_exact(&mut sync).is_ok() && sync == [0xff, 0xf8];
            if fixed && head[8..10] != head[10..12] {
                min_block = Some([head[10], head[11]]);
            }
        }
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            file,
            len,
            pos: 0,
            min_block,
        })
    }
}

impl std::io::Read for FlacFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        if let Some(fix) = self.min_block {
            for (i, byte) in fix.iter().enumerate() {
                let at = Self::MIN_BLOCK_AT + i as u64;
                if (self.pos..self.pos + n as u64).contains(&at) {
                    buf[(at - self.pos) as usize] = *byte;
                }
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for FlacFile {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.pos = self.file.seek(to)?;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for FlacFile {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

fn open_format(path: &Path) -> Result<(Box<dyn FormatReader>, u32), String> {
    let failed = |error: std::io::Error| format!("{}: {error}", path.display());
    let flac = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("flac"));
    let source: Box<dyn symphonia::core::io::MediaSource> = if flac {
        Box::new(FlacFile::open(path).map_err(failed)?)
    } else {
        Box::new(std::fs::File::open(path).map_err(failed)?)
    };
    let stream = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| format!("{}: no audio in the file", path.display()))?
        .id;
    Ok((format, track))
}

/// A file's channels, rate and length, from its header (counted by reading
/// it through when the header does not say).
pub fn probe_file(path: &Path) -> Result<TakeFile, String> {
    let (mut format, track_id) = open_format(path)?;
    let params = format
        .tracks()
        .iter()
        .find(|t| t.id == track_id)
        .map(|t| t.codec_params.clone())
        .ok_or_else(|| format!("{}: no audio in the file", path.display()))?;
    let rate = params
        .sample_rate
        .ok_or_else(|| format!("{}: no sample rate in the header", path.display()))?;
    let channels = params.channels.map(|c| c.count()).unwrap_or(0);
    if channels == 0 {
        return Err(format!("{}: no channels in the header", path.display()));
    }
    let frames = match params.n_frames {
        Some(frames) => frames,
        None => {
            let mut frames = 0u64;
            loop {
                match format.next_packet() {
                    Ok(packet) if packet.track_id() == track_id => frames += packet.dur,
                    Ok(_) => {}
                    Err(SymphoniaError::IoError(error))
                        if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        break;
                    }
                    Err(error) => return Err(format!("{}: {error}", path.display())),
                }
            }
            frames
        }
    };
    Ok(TakeFile {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: path.to_path_buf(),
        channels: u16::try_from(channels).unwrap_or(u16::MAX),
        rate,
        seconds: frames as f64 / f64::from(rate.max(1)),
        frames,
    })
}

/// The WAV and FLAC files directly in `folder`, by name.
fn audio_files(folder: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        std::fs::read_dir(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file() && is_audio_file(path))
        .collect();
    files.sort_by_key(|path| path.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    Ok(files)
}

/// Every file of the take in `folder`. Fails on a file that cannot be read,
/// or when there is none.
pub fn probe_take(folder: &Path) -> Result<Vec<TakeFile>, String> {
    let files = audio_files(folder)?
        .iter()
        .map(|path| probe_file(path))
        .collect::<Result<Vec<_>, _>>()?;
    if files.is_empty() {
        return Err(format!("{}: no WAV or FLAC files", folder.display()));
    }
    Ok(files)
}

/// The take folders in `root` (the recordings folder): each folder holding
/// at least one readable WAV or FLAC file, newest first. Files that cannot
/// be read are left out.
pub fn list_takes(root: &Path) -> Vec<TakeInfo> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut takes: Vec<(SystemTime, TakeInfo)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let path = entry.path();
            let files: Vec<TakeFile> = audio_files(&path)
                .ok()?
                .iter()
                .filter_map(|file| probe_file(file).ok())
                .collect();
            if files.is_empty() {
                return None;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((
                modified,
                TakeInfo {
                    name: entry.file_name().to_string_lossy().to_string(),
                    path,
                    files,
                },
            ))
        })
        .collect();
    takes.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.name.cmp(&a.1.name)));
    takes.into_iter().map(|(_, take)| take).collect()
}

/// Each file to a channel: the one it was assigned in `saved` (a show
/// loaded again) when that channel still exists, else the channel whose
/// name gives the file's stem as the recorder names its files (ignoring
/// case), else none. A channel takes one file at most.
pub(crate) fn assign_tracks(
    files: &[TakeFile],
    channels: &[ChannelStrip],
    saved: &[PlaybackTrack],
) -> Vec<PlaybackTrack> {
    let mut taken = Vec::new();
    let mut tracks: Vec<PlaybackTrack> = files
        .iter()
        .map(|file| {
            let channel = saved
                .iter()
                .find(|t| t.file == file.name)
                .and_then(|t| t.channel)
                .filter(|id| channels.iter().any(|c| c.id == *id))
                .filter(|id| !taken.contains(id));
            if let Some(id) = channel {
                taken.push(id);
            }
            PlaybackTrack {
                file: file.name.clone(),
                channels: file.channels,
                channel,
            }
        })
        .collect();
    // Saved assignments first, so a stem match never takes their channel.
    let restored = !saved.is_empty();
    for (track, file) in tracks.iter_mut().zip(files) {
        if track.channel.is_some() || (restored && saved.iter().any(|t| t.file == file.name)) {
            continue;
        }
        let stem = Path::new(&file.name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let found = channels.iter().find(|c| {
            !taken.contains(&c.id) && crate::recorder::file_stem(&c.name).to_lowercase() == stem
        });
        if let Some(channel) = found {
            taken.push(channel.id);
            track.channel = Some(channel.id);
        }
    }
    tracks
}

const STOPPED: u32 = 0;
const PLAYING: u32 = 1;
const PAUSED: u32 = 2;

/// The transport's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Play,
    Pause,
    /// Stop and go back to the start.
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// Where playback stands: live state, never saved.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct PlaybackStatus {
    pub state: PlaybackState,
    /// Seconds from the start of the take.
    pub position: f64,
    /// The longest file's length, seconds (0 with no take loaded).
    pub duration: f64,
    /// Blocks the reader did not keep up with (played as silence).
    pub underruns: u64,
    /// Why the take did not load, or a file stopped reading.
    pub error: Option<String>,
    /// The whole take repeats.
    #[serde(rename = "loop")]
    pub looping: bool,
}

/// One file's ring: interleaved at 1 (a mono file) or 2 channels (any
/// other; a file of more than two plays its first two).
struct TrackRing {
    ring: SampleRing,
    channels: usize,
}

/// What only the audio thread touches.
struct AudioSide {
    /// The generation whose locate the position was last set from.
    seen_generation: u64,
    /// The last flush request carried out.
    flushed: u64,
    /// Per file, this block: `MAX_BLOCK` left then `MAX_BLOCK` right.
    planes: Box<[f32]>,
    /// One block popped from a ring, interleaved.
    scratch: Box<[f32]>,
}

/// A loaded take as the audio thread and the reader thread share it. The
/// graph holds it; [`Player`] drives it.
pub struct PlayerCell {
    state: AtomicU32,
    looping: AtomicBool,
    virtual_soundcheck: AtomicBool,
    /// Bumped by every locate (and stop).
    generation: AtomicU64,
    /// Where the newest generation starts, frames.
    locate_frame: AtomicU64,
    /// Reader → audio: empty the rings for this generation.
    flush_request: AtomicU64,
    /// Audio → reader: the rings were emptied for this generation.
    flushed: AtomicU64,
    /// Reader → audio: the rings hold this generation's audio.
    ready: AtomicU64,
    /// Frames from the start of the take.
    position: AtomicU64,
    underruns: AtomicU64,
    /// The end was reached (no loop).
    ended: AtomicBool,
    /// The reader thread is to finish.
    stop: AtomicBool,
    duration: u64,
    sample_rate: u32,
    /// The virtual soundcheck crossfade's change per sample.
    pub(crate) fade_step: f32,
    rings: Box<[TrackRing]>,
    audio: UnsafeCell<AudioSide>,
}

// SAFETY: `audio` is touched only by the audio thread (exactly one graph is
// live there at a time); each ring has the reader thread as its only
// producer and the audio thread as its only consumer. Everything else is
// atomics.
unsafe impl Sync for PlayerCell {}
unsafe impl Send for PlayerCell {}

impl PlayerCell {
    /// Stopped at the start, rings empty: one ring per file of `channels`
    /// (1 or 2). Control thread; allocates.
    fn new(channels: &[usize], duration: u64, sample_rate: u32) -> Self {
        let sr = sample_rate.max(1);
        let rings: Box<[TrackRing]> = channels
            .iter()
            .map(|&channels| TrackRing {
                ring: SampleRing::new(channels * RING_SECONDS * sr as usize),
                channels,
            })
            .collect();
        Self {
            state: AtomicU32::new(STOPPED),
            looping: AtomicBool::new(false),
            virtual_soundcheck: AtomicBool::new(false),
            // Generation 1 needs no flush: nothing stale is in the rings.
            generation: AtomicU64::new(1),
            locate_frame: AtomicU64::new(0),
            flush_request: AtomicU64::new(1),
            flushed: AtomicU64::new(1),
            ready: AtomicU64::new(0),
            position: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            duration,
            sample_rate: sr,
            fade_step: 1.0 / (BYPASS_FADE_SECONDS * sr as f32).max(1.0),
            audio: UnsafeCell::new(AudioSide {
                seen_generation: 1,
                flushed: 1,
                planes: vec![0.0; rings.len() * 2 * MAX_BLOCK].into_boxed_slice(),
                scratch: vec![0.0; 2 * MAX_BLOCK].into_boxed_slice(),
            }),
            rings,
        }
    }

    /// How many files the take has.
    pub fn tracks(&self) -> usize {
        self.rings.len()
    }

    pub(crate) fn virtual_soundcheck(&self) -> bool {
        self.virtual_soundcheck.load(Ordering::Relaxed)
    }

    /// Pop this block of every file into its planes: silence while stopped,
    /// paused, past the end or waiting for a locate to refill, and silence
    /// plus a counted underrun for what the reader did not deliver in time.
    /// Audio thread only; allocation-free.
    pub(crate) fn render(&self, n: usize) {
        // SAFETY: see the type's contract.
        let side = unsafe { &mut *self.audio.get() };
        let n = n.min(MAX_BLOCK);
        let request = self.flush_request.load(Ordering::Acquire);
        if request != side.flushed {
            for track in self.rings.iter() {
                track.ring.skip(track.ring.len());
            }
            side.flushed = request;
            self.flushed.store(request, Ordering::Release);
        }
        let generation = self.generation.load(Ordering::Acquire);
        if generation != side.seen_generation {
            side.seen_generation = generation;
            self.position
                .store(self.locate_frame.load(Ordering::Acquire), Ordering::Relaxed);
        }
        for plane in side.planes.chunks_exact_mut(MAX_BLOCK) {
            plane[..n].fill(0.0);
        }
        if self.state.load(Ordering::Relaxed) != PLAYING
            || self.ready.load(Ordering::Acquire) != generation
        {
            return;
        }
        let looping = self.looping.load(Ordering::Relaxed);
        let mut position = self.position.load(Ordering::Relaxed);
        if position >= self.duration && !looping {
            self.ended.store(true, Ordering::Relaxed);
            return;
        }
        let want = if looping {
            n
        } else {
            n.min(usize::try_from(self.duration - position).unwrap_or(usize::MAX))
        };
        let available = self
            .rings
            .iter()
            .map(|t| t.ring.len() / t.channels)
            .min()
            .unwrap_or(0);
        let frames = available.min(want);
        if frames < want {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
        for (index, track) in self.rings.iter().enumerate() {
            let samples = &mut side.scratch[..frames * track.channels];
            track.ring.pop(samples);
            let base = index * 2 * MAX_BLOCK;
            let (left, right) = side.planes[base..base + 2 * MAX_BLOCK].split_at_mut(MAX_BLOCK);
            if track.channels == 1 {
                left[..frames].copy_from_slice(samples);
                right[..frames].copy_from_slice(samples);
            } else {
                for (i, frame) in samples.chunks_exact(2).enumerate() {
                    left[i] = frame[0];
                    right[i] = frame[1];
                }
            }
        }
        position += frames as u64;
        if position >= self.duration {
            if looping && self.duration > 0 {
                position %= self.duration;
            } else {
                self.ended.store(true, Ordering::Relaxed);
            }
        }
        self.position.store(position, Ordering::Relaxed);
    }

    /// File `track`'s block from the last [`Self::render`]: `(left, right)`
    /// (a mono file on both). Audio thread only.
    pub(crate) fn planes(&self, track: usize, n: usize) -> (&[f32], &[f32]) {
        // SAFETY: see the type's contract; `render` is done with it.
        let side = unsafe { &*self.audio.get() };
        let base = track * 2 * MAX_BLOCK;
        let n = n.min(MAX_BLOCK);
        (
            &side.planes[base..base + n],
            &side.planes[base + MAX_BLOCK..base + MAX_BLOCK + n],
        )
    }
}

/// One file being decoded. Reader thread (opened on the control thread).
struct TrackReader {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    /// The file's length, frames.
    frames: u64,
    /// The ring's channels (1 or 2).
    channels: usize,
    buffer: Option<SampleBuffer<f32>>,
    /// Decoded, not yet handed out: interleaved at `channels`.
    pending: Vec<f32>,
    pending_at: usize,
    /// Frames still to drop after a seek that landed early.
    discard: u64,
    eof: bool,
}

impl TrackReader {
    fn open(file: &TakeFile) -> Result<Self, String> {
        let (format, track_id) = open_format(&file.path)?;
        let params = format
            .tracks()
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.codec_params.clone())
            .ok_or_else(|| format!("{}: no audio in the file", file.path.display()))?;
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|error| format!("{}: {error}", file.path.display()))?;
        Ok(Self {
            path: file.path.clone(),
            format,
            decoder,
            track_id,
            frames: file.frames,
            channels: if file.channels == 1 { 1 } else { 2 },
            buffer: None,
            pending: Vec::with_capacity(CHUNK_FRAMES * 2),
            pending_at: 0,
            discard: 0,
            eof: false,
        })
    }

    /// Go to `frame` (past the end: silence from now on).
    fn seek(&mut self, frame: u64) -> Result<(), String> {
        self.pending.clear();
        self.pending_at = 0;
        self.discard = 0;
        self.eof = false;
        if frame >= self.frames {
            self.eof = true;
            return Ok(());
        }
        let to = SeekTo::TimeStamp {
            ts: frame,
            track_id: self.track_id,
        };
        match self.format.seek(SeekMode::Accurate, to) {
            Ok(seeked) => {
                self.decoder.reset();
                self.discard = seeked.required_ts.saturating_sub(seeked.actual_ts);
                Ok(())
            }
            Err(_) => {
                // Read it through from the start instead.
                let file = TakeFile {
                    name: String::new(),
                    path: self.path.clone(),
                    channels: self.channels as u16,
                    rate: 0,
                    seconds: 0.0,
                    frames: self.frames,
                };
                *self = Self::open(&file)?;
                self.discard = frame;
                Ok(())
            }
        }
    }

    /// Fill `out` (interleaved at the ring's channels) with the next frames;
    /// past the end, or once the file failed, silence.
    fn read(&mut self, out: &mut [f32]) -> Result<(), String> {
        let mut filled = 0;
        while filled < out.len() {
            if self.pending_at < self.pending.len() {
                let n = (self.pending.len() - self.pending_at).min(out.len() - filled);
                out[filled..filled + n]
                    .copy_from_slice(&self.pending[self.pending_at..self.pending_at + n]);
                filled += n;
                self.pending_at += n;
                continue;
            }
            if self.eof {
                out[filled..].fill(0.0);
                return Ok(());
            }
            if let Err(error) = self.decode_next() {
                self.eof = true;
                out[filled..].fill(0.0);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Decode one packet into `pending`.
    fn decode_next(&mut self) -> Result<(), String> {
        self.pending.clear();
        self.pending_at = 0;
        let packet = match self.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                self.eof = true;
                return Ok(());
            }
            Err(SymphoniaError::ResetRequired) => {
                self.decoder.reset();
                return Ok(());
            }
            Err(error) => return Err(format!("{}: {error}", self.path.display())),
        };
        if packet.track_id() != self.track_id {
            return Ok(());
        }
        let decoded = match self.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            // A damaged packet: skipped, the file plays on.
            Err(SymphoniaError::DecodeError(_)) => return Ok(()),
            Err(error) => return Err(format!("{}: {error}", self.path.display())),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let frames = decoded.frames();
        if self
            .buffer
            .as_ref()
            .is_none_or(|b| b.capacity() < frames * channels)
        {
            self.buffer = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
        }
        let buffer = self.buffer.as_mut().expect("made above");
        buffer.copy_interleaved_ref(decoded);
        let skip = usize::try_from(self.discard)
            .unwrap_or(usize::MAX)
            .min(frames);
        self.discard -= skip as u64;
        for frame in buffer.samples().chunks_exact(channels).skip(skip) {
            if self.channels == 1 {
                self.pending.push(frame[0]);
            } else {
                self.pending.push(frame[0]);
                self.pending.push(frame.get(1).copied().unwrap_or(frame[0]));
            }
        }
        Ok(())
    }
}

/// A loaded take: its shared cell, and the reader thread filling it.
/// Control thread.
pub struct Player {
    cell: Arc<PlayerCell>,
    files: Vec<TakeFile>,
    reader: Option<JoinHandle<()>>,
    /// The first error the reader met.
    error: Arc<Mutex<Option<String>>>,
}

impl Player {
    /// Open every file of a take for an engine at `sample_rate`: refused
    /// when a file was recorded at another rate (there is no resampler),
    /// when a file cannot be decoded, or when the take is empty. Allocates
    /// the rings and starts the reader, which fills them from the start.
    pub fn open(files: Vec<TakeFile>, sample_rate: u32) -> Result<Self, String> {
        if files.is_empty() {
            return Err("the take has no files".to_string());
        }
        if let Some(file) = files.iter().find(|f| f.rate != sample_rate) {
            return Err(format!(
                "{} was recorded at {} Hz and the engine runs at {} Hz: LiveStage does not \
                 resample — run the interface at {} Hz to play this take",
                file.name, file.rate, sample_rate, file.rate
            ));
        }
        let duration = files.iter().map(|f| f.frames).max().unwrap_or(0);
        if duration == 0 {
            return Err("the take is empty".to_string());
        }
        let readers = files
            .iter()
            .map(TrackReader::open)
            .collect::<Result<Vec<_>, _>>()?;
        let channels: Vec<usize> = readers.iter().map(|r| r.channels).collect();
        let cell = Arc::new(PlayerCell::new(&channels, duration, sample_rate));
        let error = Arc::new(Mutex::new(None));
        let reader = {
            let cell = cell.clone();
            let error = error.clone();
            std::thread::Builder::new()
                .name("livestage-playback".into())
                .spawn(move || read_loop(&cell, readers, &error))
                .map_err(|error| error.to_string())?
        };
        Ok(Self {
            cell,
            files,
            reader: Some(reader),
            error,
        })
    }

    pub fn cell(&self) -> &Arc<PlayerCell> {
        &self.cell
    }

    pub fn files(&self) -> &[TakeFile] {
        &self.files
    }

    pub fn transport(&self, action: Transport) {
        let cell = &self.cell;
        match action {
            Transport::Play => {
                let at_end = cell.position.load(Ordering::Relaxed) >= cell.duration;
                if cell.state.load(Ordering::Relaxed) != PLAYING
                    && at_end
                    && !cell.looping.load(Ordering::Relaxed)
                {
                    self.locate_frame(0);
                }
                cell.state.store(PLAYING, Ordering::Relaxed);
            }
            Transport::Pause => {
                let _ = cell.state.compare_exchange(
                    PLAYING,
                    PAUSED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
            Transport::Stop => {
                cell.state.store(STOPPED, Ordering::Relaxed);
                self.locate_frame(0);
            }
        }
    }

    /// Go to `seconds` from the start (clamped to the take).
    pub fn locate(&self, seconds: f64) {
        let frame = (seconds.max(0.0) * f64::from(self.cell.sample_rate)).round();
        self.locate_frame((frame as u64).min(self.cell.duration));
    }

    fn locate_frame(&self, frame: u64) {
        let cell = &self.cell;
        cell.locate_frame.store(frame, Ordering::Release);
        cell.position.store(frame, Ordering::Relaxed);
        cell.ended.store(false, Ordering::Relaxed);
        cell.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn set_loop(&self, on: bool) {
        self.cell.looping.store(on, Ordering::Relaxed);
    }

    pub fn set_virtual_soundcheck(&self, on: bool) {
        self.cell.virtual_soundcheck.store(on, Ordering::Relaxed);
    }

    /// At control rate: a take that played to its end (no loop) stops and
    /// goes back to the start.
    pub fn poll(&self) {
        if self.cell.ended.load(Ordering::Relaxed)
            && self.cell.state.load(Ordering::Relaxed) == PLAYING
        {
            self.transport(Transport::Stop);
        }
    }

    /// Whether the rings hold audio for the latest locate.
    pub fn is_ready(&self) -> bool {
        self.cell.ready.load(Ordering::Acquire) == self.cell.generation.load(Ordering::Acquire)
    }

    pub fn status(&self) -> PlaybackStatus {
        let cell = &self.cell;
        let rate = f64::from(cell.sample_rate);
        PlaybackStatus {
            state: match cell.state.load(Ordering::Relaxed) {
                PLAYING => PlaybackState::Playing,
                PAUSED => PlaybackState::Paused,
                _ => PlaybackState::Stopped,
            },
            position: cell.position.load(Ordering::Relaxed).min(cell.duration) as f64 / rate,
            duration: cell.duration as f64 / rate,
            underruns: cell.underruns.load(Ordering::Relaxed),
            error: self.error.lock().clone(),
            looping: cell.looping.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.cell.stop.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// The reader thread: keep every ring full ahead of the audio thread,
/// follow locates (see the module's handshake) and loop.
fn read_loop(cell: &PlayerCell, mut readers: Vec<TrackReader>, error: &Mutex<Option<String>>) {
    let fail = |message: String| {
        let mut slot = error.lock();
        if slot.is_none() {
            *slot = Some(message);
        }
    };
    let capacity = cell
        .rings
        .iter()
        .map(|t| t.ring.capacity() / t.channels)
        .min()
        .unwrap_or(0);
    let prefill = ((PREFILL_SECONDS * f64::from(cell.sample_rate)) as usize)
        .max(CHUNK_FRAMES)
        .min(capacity);
    let mut scratch = vec![0.0f32; CHUNK_FRAMES * 2];
    let mut generation = 0u64;
    let mut cursor = 0u64;
    let mut announced = true;
    'run: loop {
        if cell.stop.load(Ordering::Acquire) {
            return;
        }
        let wanted = cell.generation.load(Ordering::Acquire);
        if wanted != generation {
            // Nothing stale is pushed from here on: have the audio thread
            // empty the rings, then refill them from the new place.
            cell.flush_request.store(wanted, Ordering::Release);
            while cell.flushed.load(Ordering::Acquire) != wanted {
                if cell.stop.load(Ordering::Acquire) {
                    return;
                }
                if cell.generation.load(Ordering::Acquire) != wanted {
                    continue 'run;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            cursor = cell.locate_frame.load(Ordering::Acquire).min(cell.duration);
            for reader in &mut readers {
                if let Err(message) = reader.seek(cursor) {
                    fail(message);
                }
            }
            generation = wanted;
            announced = false;
        }
        if cursor >= cell.duration && cell.looping.load(Ordering::Relaxed) {
            for reader in &mut readers {
                if let Err(message) = reader.seek(0) {
                    fail(message);
                }
            }
            cursor = 0;
        }
        let free = cell
            .rings
            .iter()
            .map(|t| (t.ring.capacity() - t.ring.len()) / t.channels)
            .min()
            .unwrap_or(0);
        let remaining = usize::try_from(cell.duration - cursor).unwrap_or(usize::MAX);
        let frames = CHUNK_FRAMES.min(free).min(remaining);
        let mut moved = false;
        if frames > 0 && (frames == CHUNK_FRAMES || frames == remaining) {
            for (reader, track) in readers.iter_mut().zip(cell.rings.iter()) {
                let samples = &mut scratch[..frames * track.channels];
                if let Err(message) = reader.read(samples) {
                    fail(message);
                }
                // Fits: `free` was measured, and this is the only producer.
                track.ring.push(samples);
            }
            cursor += frames as u64;
            moved = true;
        }
        if !announced {
            let buffered = cell
                .rings
                .iter()
                .map(|t| t.ring.len() / t.channels)
                .min()
                .unwrap_or(0);
            if buffered >= prefill || cursor >= cell.duration {
                cell.ready.store(generation, Ordering::Release);
                announced = true;
            }
        }
        if !moved {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cell with no reader: the test pushes into the rings itself.
    fn cell(channels: &[usize], duration: u64) -> PlayerCell {
        let cell = PlayerCell::new(channels, duration, 48_000);
        cell.ready.store(1, Ordering::Release);
        cell.state.store(PLAYING, Ordering::Relaxed);
        cell
    }

    #[test]
    fn a_dry_ring_plays_silence_and_counts_an_underrun() {
        let cell = cell(&[1, 2], 48_000);
        cell.rings[0].ring.push(&[0.5; 100]);
        cell.rings[1].ring.push(&[0.25; 200]);
        cell.render(64);
        assert_eq!(cell.underruns.load(Ordering::Relaxed), 0);
        assert!(cell.planes(0, 64).0.iter().all(|s| *s == 0.5));
        // 36 frames left, 64 wanted: those 36, then silence.
        cell.render(64);
        assert_eq!(cell.underruns.load(Ordering::Relaxed), 1);
        let (left, right) = cell.planes(1, 64);
        assert!(left[..36].iter().all(|s| *s == 0.25));
        assert!(left[36..].iter().all(|s| *s == 0.0));
        assert!(right[36..].iter().all(|s| *s == 0.0));
        assert_eq!(cell.position.load(Ordering::Relaxed), 100);
        // Both files moved together: alignment is kept.
        assert_eq!(cell.rings[0].ring.len(), 0);
        assert_eq!(cell.rings[1].ring.len(), 0);
    }

    #[test]
    fn a_stale_generation_is_silent_and_a_flush_empties_the_rings() {
        let cell = cell(&[1], 48_000);
        cell.rings[0].ring.push(&[0.5; 512]);
        // A locate: generation 2, not ready yet.
        cell.locate_frame.store(1000, Ordering::Release);
        cell.generation.store(2, Ordering::Release);
        cell.render(64);
        assert!(cell.planes(0, 64).0.iter().all(|s| *s == 0.0));
        assert_eq!(cell.underruns.load(Ordering::Relaxed), 0, "not an underrun");
        assert_eq!(cell.position.load(Ordering::Relaxed), 1000);
        assert_eq!(cell.rings[0].ring.len(), 512, "nothing popped");
        // The reader asks for the flush.
        cell.flush_request.store(2, Ordering::Release);
        cell.render(64);
        assert_eq!(cell.rings[0].ring.len(), 0);
        assert_eq!(cell.flushed.load(Ordering::Acquire), 2);
        // Refilled and ready: plays from there.
        cell.rings[0].ring.push(&[0.75; 128]);
        cell.ready.store(2, Ordering::Release);
        cell.render(64);
        assert!(cell.planes(0, 64).0.iter().all(|s| *s == 0.75));
        assert_eq!(cell.position.load(Ordering::Relaxed), 1064);
    }

    #[test]
    fn the_end_stops_without_an_underrun_and_a_loop_wraps() {
        let cell = cell(&[1], 100);
        cell.rings[0].ring.push(&[0.5; 100]);
        cell.render(64);
        cell.render(64);
        assert_eq!(cell.underruns.load(Ordering::Relaxed), 0);
        assert!(cell.ended.load(Ordering::Relaxed));
        assert_eq!(cell.position.load(Ordering::Relaxed), 100);
        let (left, _) = cell.planes(0, 64);
        assert!(left[..36].iter().all(|s| *s == 0.5) && left[36..].iter().all(|s| *s == 0.0));

        let cell = self::cell(&[1], 100);
        cell.looping.store(true, Ordering::Relaxed);
        cell.rings[0].ring.push(&[0.5; 160]);
        cell.render(64);
        cell.render(64);
        assert!(!cell.ended.load(Ordering::Relaxed));
        assert_eq!(cell.position.load(Ordering::Relaxed), 28);
    }

    #[test]
    fn paused_or_stopped_plays_silence_and_keeps_the_rings() {
        let cell = cell(&[2], 48_000);
        cell.rings[0].ring.push(&[0.5; 256]);
        cell.state.store(PAUSED, Ordering::Relaxed);
        cell.render(64);
        assert!(cell.planes(0, 64).1.iter().all(|s| *s == 0.0));
        assert_eq!(cell.rings[0].ring.len(), 256);
        cell.state.store(STOPPED, Ordering::Relaxed);
        cell.render(64);
        assert_eq!(cell.rings[0].ring.len(), 256);
        assert_eq!(cell.underruns.load(Ordering::Relaxed), 0);
    }

    fn channel(id: crate::session::Id, name: &str) -> ChannelStrip {
        ChannelStrip::new(id, name.to_string(), crate::session::InputPatch::mono(0))
    }

    fn file(name: &str, channels: u16) -> TakeFile {
        TakeFile {
            name: name.to_string(),
            path: PathBuf::from(name),
            channels,
            rate: 48_000,
            seconds: 1.0,
            frames: 48_000,
        }
    }

    #[test]
    fn files_go_to_the_channel_their_stem_names() {
        let channels = [
            channel(1, "Kick"),
            channel(2, "Snare: top"),
            channel(3, "Vox"),
            channel(4, "kick"),
        ];
        let files = [
            file("Kick.wav", 1),
            file("Snare_ top.flac", 1),
            file("Kick 2.wav", 2),
            file("Bass.wav", 1),
            file("VOX.WAV", 2),
        ];
        let tracks = assign_tracks(&files, &channels, &[]);
        let assigned: Vec<Option<u32>> = tracks.iter().map(|t| t.channel).collect();
        // `Kick` takes the first channel of that name; the second "kick"
        // stays free (the recorder named its file "Kick 2").
        assert_eq!(assigned, [Some(1), Some(2), None, None, Some(3)]);
        assert_eq!(tracks[2].channels, 2);
        assert_eq!(tracks[0].file, "Kick.wav");
    }

    #[test]
    fn saved_assignments_win_over_stems() {
        let channels = [channel(1, "Kick"), channel(2, "Snare")];
        let files = [
            file("Kick.wav", 1),
            file("Snare.wav", 1),
            file("New.wav", 1),
        ];
        let saved = [
            PlaybackTrack {
                file: "Kick.wav".into(),
                channels: 1,
                channel: Some(2),
            },
            PlaybackTrack {
                file: "Snare.wav".into(),
                channels: 1,
                channel: None,
            },
            PlaybackTrack {
                file: "Gone.wav".into(),
                channels: 1,
                channel: Some(1),
            },
        ];
        let tracks = assign_tracks(&files, &channels, &saved);
        let assigned: Vec<Option<u32>> = tracks.iter().map(|t| t.channel).collect();
        // Kick stays on channel 2; Snare stays unassigned (it was saved so);
        // New has no channel of its stem.
        assert_eq!(assigned, [Some(2), None, None]);
    }

    #[test]
    fn a_take_at_another_rate_is_refused_naming_both() {
        let mut file = file("Kick.wav", 1);
        file.rate = 44_100;
        let error = Player::open(vec![file], 48_000).err().unwrap();
        assert!(
            error.contains("44100 Hz") && error.contains("48000 Hz"),
            "{error}"
        );
    }
}
