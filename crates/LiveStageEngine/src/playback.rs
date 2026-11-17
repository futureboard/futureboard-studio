//! Playback of a recorded take, for virtual soundcheck: each file of the
//! take feeds the channel it was recorded from, in place of the interface
//! input.
//!
//! ```txt
//! take folder ──► reader thread (mkff-audio: decode, seek, loop)
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

use mkff_audio::{AudioBuffer, AudioFile};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

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

/// Failed chunks in a row after which a take file counts as unreadable (or,
/// when the failures are its data running out, as ended).
const MAX_CONSECUTIVE_DECODE_FAILURES: u32 = 256;

/// A take file opened with mkff-audio, which reads what the recorder writes
/// as it is:
///
/// * **FLAC**: takes recorded before #352 have the last block's length as
///   STREAMINFO's minimum block size; mkff takes the fixed block size from
///   the first frame header, so they decode and seek like current takes.
/// * **WAV**: a take that outgrew 4 GiB is RF64, which mkff reads natively —
///   the whole take, not only its first 4 GiB.
fn open_file(path: &Path) -> Result<AudioFile, String> {
    AudioFile::open(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// The next decoded chunk of `file`: a damaged packet is skipped (mkff keeps
/// positions, the hole decodes as silence) and the file plays on; a run of
/// failures is the end of a truncated file, or an error.
fn next_chunk(file: &mut AudioFile, path: &Path) -> Result<Option<AudioBuffer>, String> {
    let mut failures = 0;
    loop {
        match file.next_chunk() {
            Ok(chunk) => return Ok(chunk),
            Err(error) => {
                failures += 1;
                if failures >= MAX_CONSECUTIVE_DECODE_FAILURES {
                    return match error {
                        mkff_audio::Error::EndOfData => Ok(None),
                        error => Err(format!("{}: {error}", path.display())),
                    };
                }
            }
        }
    }
}

/// A file's channels, rate and length, from its header (counted by reading
/// it through when the header does not say).
pub fn probe_file(path: &Path) -> Result<TakeFile, String> {
    let mut file = open_file(path)?;
    let rate = file.sample_rate();
    if rate == 0 {
        return Err(format!("{}: no sample rate in the header", path.display()));
    }
    let channels = file.channels();
    if channels == 0 {
        return Err(format!("{}: no channels in the header", path.display()));
    }
    let frames = match file.duration() {
        Some(frames) => frames,
        None => {
            let mut frames = 0u64;
            while let Some(chunk) = next_chunk(&mut file, path)? {
                frames += chunk.frames() as u64;
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
    file: AudioFile,
    /// The file's length, frames.
    frames: u64,
    /// The ring's channels (1 or 2).
    channels: usize,
    /// Decoded, not yet handed out: interleaved at `channels`.
    pending: Vec<f32>,
    pending_at: usize,
    eof: bool,
}

impl TrackReader {
    fn open(file: &TakeFile) -> Result<Self, String> {
        Ok(Self {
            path: file.path.clone(),
            file: open_file(&file.path)?,
            frames: file.frames,
            channels: if file.channels == 1 { 1 } else { 2 },
            pending: Vec::with_capacity(CHUNK_FRAMES * 2),
            pending_at: 0,
            eof: false,
        })
    }

    /// Go to `frame` (past the end: silence from now on). mkff seeks
    /// sample-exactly, so nothing has to be dropped after it.
    fn seek(&mut self, frame: u64) -> Result<(), String> {
        self.pending.clear();
        self.pending_at = 0;
        self.eof = false;
        if frame >= self.frames {
            self.eof = true;
            return Ok(());
        }
        if let Err(error) = self.file.seek(frame) {
            // Silence rather than audio from the wrong place.
            self.eof = true;
            return Err(format!("{}: {error}", self.path.display()));
        }
        Ok(())
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

    /// Decode one chunk into `pending`.
    fn decode_next(&mut self) -> Result<(), String> {
        self.pending.clear();
        self.pending_at = 0;
        let Some(chunk) = next_chunk(&mut self.file, &self.path)? else {
            self.eof = true;
            return Ok(());
        };
        let left = chunk.channels.first().map_or(&[][..], Vec::as_slice);
        let right = chunk.channels.get(1).map_or(left, Vec::as_slice);
        if self.channels == 1 {
            self.pending.extend_from_slice(left);
        } else {
            for (&l, &r) in left.iter().zip(right) {
                self.pending.push(l);
                self.pending.push(r);
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

    /// A float WAV as the recorder writes it once a take outgrows RIFF:
    /// `RF64`, `ds64` with the real sizes, and `0xFFFFFFFF` in the RIFF and
    /// data size fields. Small here; only the header matters.
    fn rf64_file(path: &Path, channels: u16, samples: &[f32]) {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let frames = (samples.len() / usize::from(channels)) as u64;
        let block = channels * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RF64");
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"ds64");
        bytes.extend_from_slice(&28u32.to_le_bytes());
        bytes.extend_from_slice(&(72 + data.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&frames.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&(48_000 * u32::from(block)).to_le_bytes());
        bytes.extend_from_slice(&block.to_le_bytes());
        bytes.extend_from_slice(&32u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&data);
        std::fs::write(path, bytes).unwrap();
    }

    /// A take that outgrew 4 GiB is RF64: it opens with its real length,
    /// plays its samples through the reader, and seeks to the exact frame.
    #[test]
    fn an_rf64_take_opens_with_its_length_and_samples() {
        let dir = std::env::temp_dir().join(format!("livestage-rf64-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Kick.wav");
        let samples: Vec<f32> = (0..2000).map(|i| i as f32 / 4000.0).collect();
        rf64_file(&path, 2, &samples);

        let file = probe_file(&path).unwrap();
        assert_eq!((file.channels, file.rate, file.frames), (2, 48_000, 1000));

        let mut reader = TrackReader::open(&file).unwrap();
        let mut decoded = vec![0.0f32; samples.len()];
        reader.read(&mut decoded).unwrap();
        assert_eq!(decoded, samples);

        reader.seek(617).unwrap();
        let mut frame = [0.0f32; 4];
        reader.read(&mut frame).unwrap();
        assert_eq!(frame[..], samples[617 * 2..619 * 2]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #388: a FLAC take recorded before #352 — STREAMINFO's minimum block
    /// size is the last block's length — probes with its length, plays and
    /// seeks like the same take with the current header, read as it is.
    #[test]
    fn a_pre_352_flac_take_plays_like_a_current_one() {
        use sphere_encoder::{
            AudioEncodeOptions, AudioEncodeSpec, AudioFileFormat, AudioSampleFormat, create_encoder,
        };
        let dir = std::env::temp_dir().join(format!("livestage-pre352-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (current, old) = (dir.join("Keys.flac"), dir.join("Keys old.flac"));
        let frames = 65_541usize;
        let mut encoder = create_encoder(
            &current,
            AudioEncodeSpec {
                sample_rate: 48_000,
                channels: 2,
                sample_format: AudioSampleFormat::I16,
            },
            AudioEncodeOptions {
                format: AudioFileFormat::Flac,
                ..AudioEncodeOptions::default()
            },
        )
        .unwrap();
        let samples: Vec<f32> = (0..frames * 2)
            .map(|i| ((i / 2) as f32 * 0.01 + (i % 2) as f32).sin() * 0.5)
            .collect();
        encoder.write_interleaved_f32(&samples).unwrap();
        encoder.finalize().unwrap();
        let mut bytes = std::fs::read(&current).unwrap();
        bytes[8..10].copy_from_slice(&((frames % 4_096) as u16).to_be_bytes());
        std::fs::write(&old, &bytes).unwrap();
        assert!(
            sphere_encoder::flac_header::legacy_min_block_size_fix(
                &mut std::fs::File::open(&old).unwrap()
            )
            .is_some(),
            "the fixture is not a pre-#352 header"
        );

        let play = |path: &Path, from: u64| {
            let file = probe_file(path).unwrap();
            assert_eq!((file.channels, file.frames), (2, frames as u64));
            let mut reader = TrackReader::open(&file).unwrap();
            reader.seek(from).unwrap();
            let mut out = vec![0.0f32; (frames - from as usize) * 2];
            reader.read(&mut out).unwrap();
            out
        };
        for from in [0u64, 4_096 * 9 + 123] {
            let want = play(&current, from);
            assert_eq!(want.len(), (frames - from as usize) * 2);
            assert!(play(&old, from) == want, "from {from}");
            for (got, want) in want.iter().zip(&samples[from as usize * 2..]) {
                assert!((got - want).abs() < 1e-4, "from {from}: {got} vs {want}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
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
