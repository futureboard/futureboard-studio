use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};

use filmcraft_media::reader::SharedReader;
use filmcraft_media::{FrameRequest, SharedSource};
use filmcraft_time::{FrameRate, Tick};
use mkff_audio::AudioFile;

use crate::{AudioBuffer, Error, Frame, Result};

/// What a [`MediaFile`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// Video, possibly with audio.
    Movie,
    AudioOnly,
    Still,
    ImageSequence,
}

/// The video stream of a [`MediaFile`].
#[derive(Clone, Debug, PartialEq)]
pub struct VideoStreamInfo {
    /// Codec name as the container reports it ("H.264", "HEVC", "VP9", "ProRes 422 HQ", …).
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction (`num / den`).
    pub frame_rate: (i64, i64),
    /// Pixel aspect ratio.
    pub pixel_aspect: (u32, u32),
    /// Pixel format label ("4:2:0 8-bit", "4:2:2 10-bit", …).
    pub pixel_format: String,
    pub has_alpha: bool,
    /// Bits per second, when known.
    pub bitrate: Option<u64>,
}

/// One audio stream of a [`MediaFile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioStreamInfo {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: Option<u32>,
}

/// What [`MediaFile::info`] reports.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaInfo {
    pub name: String,
    /// Container name ("MP4", "Matroska", "MPEG-TS", …).
    pub container: String,
    pub kind: MediaKind,
    pub duration_seconds: f64,
    pub video: Option<VideoStreamInfo>,
    /// Every audio stream, in file order (index with [`MediaFile::audio_stream`]).
    pub audio: Vec<AudioStreamInfo>,
    pub file_size: Option<u64>,
}

/// Bytes shared between the readers of an in-memory file.
#[derive(Clone)]
struct SharedBytes(Arc<[u8]>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Where a stream's audio comes from.
enum AudioSource {
    /// MKFF's own demuxer + decoder (`mkff-audio`).
    Own(Box<Mutex<AudioFile>>),
    /// The FilmCraft source's stream (containers MKFF does not demux itself yet).
    FilmCraft,
}

/// A media file opened for random-access decoding: any container and codec this crate supports
/// (see the crate docs), detected from the file's contents.
///
/// Audio is MKFF's own (`mkff-audio`) for WAV / AIFF, MP3, ADTS, FLAC, Ogg and MP4 / MOV: those
/// files never touch FilmCraft unless they also carry video. Video, and the audio of the
/// containers MKFF does not demux itself yet (Matroska / WebM, AVI, MXF, MPEG-TS / PS), still
/// comes through the FilmCraft source.
///
/// Video is read by frame index or time. Seeking decodes forward from the preceding sync sample
/// and caches the rest of the GOP, so reading frames in order is as fast as streaming. Audio is
/// read as sample ranges, resampled to any rate; reading forward in order streams.
///
/// `MediaFile` is `Send + Sync`; concurrent requests are serialised per stream internally.
#[derive(Clone)]
pub struct MediaFile {
    /// FilmCraft source for video (and FilmCraft-only audio); `None` for audio-only files.
    src: Option<SharedSource>,
    info: MediaInfo,
    audio: Arc<[AudioSource]>,
}

fn container_name(format: &str) -> String {
    match format {
        "MP4" => "MPEG-4".into(),
        other => other.into(),
    }
}

impl MediaFile {
    /// Open a file from disk. Containers with an index (MP4 / MOV, Matroska / WebM, MXF, AVI,
    /// MPEG-TS / PS, Ogg, FLAC, MP3) are read in place: only the index now, samples on demand.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let file_size = std::fs::metadata(path).ok().map(|m| m.len());
        let open_audio = |i: usize| AudioFile::open_track(path, i);
        let own = mkff_audio::format::open_path(path).ok();
        Self::assemble(&name, file_size, own, &open_audio, || {
            let reader = filmcraft_media::reader::FileReader::open(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
            let reader: SharedReader = Arc::new(reader);
            Ok(filmcraft_media::reader::open_reader(
                &name,
                Arc::clone(&reader),
                &filmcraft_codecs::reader_openers(),
                &filmcraft_codecs::openers(),
            )?)
        })
    }

    /// Open media held in memory. `name` (a file name) only breaks ties for formats that can't be
    /// told apart by content.
    pub fn from_bytes(name: &str, bytes: impl Into<Arc<[u8]>>) -> Result<Self> {
        let bytes: Arc<[u8]> = bytes.into();
        let reader = |b: &Arc<[u8]>| Box::new(Cursor::new(SharedBytes(Arc::clone(b)))) as Box<dyn mkff_audio::format::ReadSeek>;
        let own = mkff_audio::format::open(reader(&bytes), name).ok();
        let open_audio = |i: usize| AudioFile::from_reader(reader(&bytes), name, i);
        let size = Some(bytes.len() as u64);
        Self::assemble(name, size, own, &open_audio, || {
            Ok(filmcraft_media::open_bytes(name, Arc::clone(&bytes), &filmcraft_codecs::openers())?)
        })
    }

    fn assemble(
        name: &str,
        file_size: Option<u64>,
        own: Option<Box<dyn mkff_audio::format::Demuxer>>,
        open_audio: &dyn Fn(usize) -> mkff_audio::Result<AudioFile>,
        filmcraft: impl FnOnce() -> Result<SharedSource>,
    ) -> Result<Self> {
        // MKFF's own audio, when its demuxer takes the file and finds audio
        let own = own.filter(|d| !d.tracks().is_empty());
        let Some(demux) = own else {
            return Ok(Self::from_source(filmcraft()?));
        };
        let format = demux.format_name().to_owned();
        let count = demux.tracks().len();
        let mut files = Vec::with_capacity(count);
        files.push(AudioFile::from_demuxer(demux, 0)?);
        for i in 1..count {
            files.push(open_audio(i)?);
        }
        let streams: Vec<AudioStreamInfo> = files
            .iter()
            .map(|f| AudioStreamInfo {
                codec: f.track().name.clone(),
                sample_rate: f.sample_rate(),
                channels: f.channels() as u32,
                bits_per_sample: f.track().bits_per_sample,
            })
            .collect();
        let audio_secs = files.iter().filter_map(AudioFile::duration_secs).fold(0.0, f64::max);
        let audio: Arc<[AudioSource]> = files.into_iter().map(|f| AudioSource::Own(Box::new(Mutex::new(f)))).collect();

        // MP4 / MOV may also carry video, which FilmCraft decodes (audio-only extensions skip it)
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        if format == "MP4" && !matches!(ext.as_str(), "m4a" | "m4b" | "m4r" | "3ga") {
            if let Ok(src) = filmcraft() {
                let mut m = Self::from_source(src);
                if m.info.video.is_some() {
                    m.info.audio = streams;
                    m.audio = audio;
                    m.info.duration_seconds = m.info.duration_seconds.max(audio_secs);
                    return Ok(m);
                }
            }
        }
        Ok(MediaFile {
            src: None,
            info: MediaInfo {
                name: name.to_owned(),
                container: container_name(&format),
                kind: MediaKind::AudioOnly,
                duration_seconds: audio_secs,
                video: None,
                audio: streams,
                file_size,
            },
            audio,
        })
    }

    /// Wrap an already-opened FilmCraft media source (video and audio both through FilmCraft).
    pub fn from_source(src: SharedSource) -> Self {
        let i = src.info();
        let info = MediaInfo {
            name: i.name.clone(),
            container: i.container.clone(),
            kind: match i.kind {
                filmcraft_media::MediaKind::AudioOnly => MediaKind::AudioOnly,
                filmcraft_media::MediaKind::Still => MediaKind::Still,
                filmcraft_media::MediaKind::ImageSequence => MediaKind::ImageSequence,
                _ => MediaKind::Movie,
            },
            duration_seconds: i.duration.seconds(),
            video: i.video.as_ref().map(|v| VideoStreamInfo {
                codec: v.codec.clone(),
                width: v.width,
                height: v.height,
                frame_rate: (v.frame_rate.num, v.frame_rate.den),
                pixel_aspect: v.par,
                pixel_format: v.pixel_format.clone(),
                has_alpha: v.has_alpha,
                bitrate: v.bitrate,
            }),
            audio: i
                .audio_streams
                .iter()
                .map(|a| AudioStreamInfo {
                    codec: a.codec.clone(),
                    sample_rate: a.sample_rate,
                    channels: a.channels,
                    bits_per_sample: a.bits_per_sample,
                })
                .collect(),
            file_size: i.file_size,
        };
        let audio = (0..info.audio.len()).map(|_| AudioSource::FilmCraft).collect();
        Self { src: Some(src), info, audio }
    }

    pub fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_src(&self) -> Result<&SharedSource> {
        match (&self.src, &self.info.video) {
            (Some(s), Some(_)) => Ok(s),
            _ => Err(Error::NoStream("video")),
        }
    }

    fn frame_rate(&self) -> Result<FrameRate> {
        self.video_src()?;
        let v = self.info.video.as_ref().ok_or(Error::NoStream("video"))?;
        Ok(FrameRate::new(v.frame_rate.0, v.frame_rate.1))
    }

    /// Number of video frames (duration × frame rate); 0 without video. The duration is the
    /// file's, so when audio runs past the last picture the final indices repeat it.
    pub fn frame_count(&self) -> u64 {
        let (Ok(rate), Ok(src)) = (self.frame_rate(), self.video_src()) else {
            return 0;
        };
        let d = src.info().duration;
        // a duration that ends inside a frame still shows that frame
        let n = rate.frame_at(d);
        let n = if rate.tick_of(n) < d { n + 1 } else { n };
        u64::try_from(n).unwrap_or(0)
    }

    /// The video frame with index `index` (0-based, at the stream's frame rate). The frame's
    /// [`Frame::pts`] is its media time in nanoseconds.
    pub fn video_frame(&self, index: u64) -> Result<Frame> {
        let rate = self.frame_rate()?;
        let index = i64::try_from(index).map_err(|_| Error::InvalidArgument(format!("frame {index}")))?;
        // The middle of the frame's interval: container timestamps are often rounded (Matroska /
        // WebM to 1 ms, so frame 1 of 24 fps video is stamped 42 ms, after 1/24 s).
        let mid = rate.tick_of(index) + Tick(rate.frame_duration().0 / 2);
        self.frame_at_tick(mid)
    }

    /// The video frame displayed at `seconds` of media time.
    pub fn video_frame_at(&self, seconds: f64) -> Result<Frame> {
        if !seconds.is_finite() {
            return Err(Error::InvalidArgument(format!("time {seconds}")));
        }
        self.frame_rate()?;
        self.frame_at_tick(Tick::from_seconds_f64(seconds.max(0.0)))
    }

    fn frame_at_tick(&self, t: Tick) -> Result<Frame> {
        let f = self.video_src()?.video_frame(FrameRequest::full(t))?;
        Ok(Frame::from_shared(f))
    }

    /// `frames` samples of audio stream 0 starting at sample `start` (at `sample_rate`), at
    /// `sample_rate` (0: the stream's own rate). Ranges past either end are padded with silence.
    pub fn audio(&self, start: i64, frames: usize, sample_rate: u32) -> Result<AudioBuffer> {
        self.audio_stream(0, start, frames, sample_rate)
    }

    /// Like [`Self::audio`] for audio stream `stream` (an index into [`MediaInfo::audio`]).
    pub fn audio_stream(&self, stream: usize, start: i64, frames: usize, sample_rate: u32) -> Result<AudioBuffer> {
        let info = self.info.audio.get(stream).ok_or(Error::NoStream("audio"))?;
        let rate = if sample_rate == 0 { info.sample_rate } else { sample_rate };
        if rate == 0 {
            return Err(Error::InvalidArgument("sample rate 0".into()));
        }
        match self.audio.get(stream) {
            Some(AudioSource::Own(f)) => {
                let mut f = f.lock().unwrap_or_else(|e| e.into_inner());
                Ok(f.read(start, frames, rate)?.into())
            }
            Some(AudioSource::FilmCraft) => {
                let src = self.src.as_ref().ok_or(Error::NoStream("audio"))?;
                let b = src.audio_stream(stream, start, frames, rate)?;
                Ok(AudioBuffer { sample_rate: b.sample_rate, channels: b.channels })
            }
            None => Err(Error::NoStream("audio")),
        }
    }

    /// Whether audio stream `stream` is decoded by MKFF's own audio stack (`mkff-audio`) rather
    /// than through FilmCraft.
    pub fn audio_is_native(&self, stream: usize) -> bool {
        matches!(self.audio.get(stream), Some(AudioSource::Own(_)))
    }

    /// The underlying FilmCraft media source (`None` for audio-only files MKFF reads itself).
    pub fn source(&self) -> Option<&SharedSource> {
        self.src.as_ref()
    }
}

impl std::fmt::Debug for MediaFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFile").field("info", &self.info).finish()
    }
}
