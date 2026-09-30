//! Export settings model: the plain, GPUI-free contract between the export
//! window's controls and the engine's [`ArrangementExportRequest`].
//!
//! Nothing here touches the timeline, the engine runtime, or GPUI — the window
//! edits an `ExportSettings`, validates it, and converts it to an engine request
//! against a plain [`EngineProjectSnapshot`] + [`ExportProjectDefaults`].

use std::path::PathBuf;

use sphere_encoder::{
    AudioEncodeOptions, AudioFileFormat, AudioSampleFormat, FlacEncodeOptions, Mp3Bitrate,
    Mp3EncodeOptions,
};
use DirectAudio::types::EngineProjectSnapshot;
use DirectAudio::{
    arrangement_bounds_samples, beats_to_samples, ArrangementExportRequest, ExportNormalizeMode,
    ExportTailMode, OfflineRenderRequest, RenderJob, TrackExportTarget,
};

/// Output sample-rate choice in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSampleRateChoice {
    Project,
    Hz44100,
    Hz48000,
    Hz88200,
    Hz96000,
}

impl ExportSampleRateChoice {
    pub fn resolve(self, project_sample_rate: u32) -> u32 {
        match self {
            Self::Project => project_sample_rate.max(1),
            Self::Hz44100 => 44_100,
            Self::Hz48000 => 48_000,
            Self::Hz88200 => 88_200,
            Self::Hz96000 => 96_000,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Hz44100 => "44100 Hz",
            Self::Hz48000 => "48000 Hz",
            Self::Hz88200 => "88200 Hz",
            Self::Hz96000 => "96000 Hz",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportChannelMode {
    Stereo,
    Mono,
}

/// How a render is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportRenderMode {
    /// Rendered offline, as fast as the machine allows, at any sample rate.
    Offline,
    /// Recorded while the project plays through the live engine, in real
    /// time, at the device rate: the way to capture what only plays live —
    /// hardware inserts, external instruments, plug-ins that render
    /// differently offline.
    Realtime,
}

impl ExportRenderMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "Offline",
            Self::Realtime => "Realtime",
        }
    }
}

/// Quick picks for the channel list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportChannelPreset {
    /// Every mixer channel: tracks, buses, returns, groups.
    All,
    /// Only the channels that make sound of their own (not routing channels),
    /// plus VSTi separate outputs — a multitrack set.
    SourceTracks,
    None,
}

/// A mixer channel the render can write a file for.
#[derive(Debug, Clone)]
pub struct ExportTrackTarget {
    pub id: String,
    pub name: String,
    /// A source channel (not a bus, return or group) or a VSTi separate
    /// output: what [`ExportChannelPreset::SourceTracks`] picks.
    pub include_in_multitrack: bool,
    /// What kind of channel it is, for the list ("Audio", "Bus", …).
    pub kind_label: String,
}

impl ExportChannelMode {
    pub fn channels(self) -> u16 {
        match self {
            Self::Stereo => 2,
            Self::Mono => 1,
        }
    }
}

/// Range to export. Beat ranges are resolved to samples against the snapshot's
/// tempo map at conversion time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportRangeChoice {
    EntireArrangement,
    TimeSelection { start_beat: f64, end_beat: f64 },
    LoopRange { start_beat: f64, end_beat: f64 },
    Custom { start_beat: f64, end_beat: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportNormalizeChoice {
    Off,
    /// Peak-normalize so the loudest sample hits the given dBFS (UI default -1.0).
    PeakDb(f32),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportTailChoice {
    None,
    FixedSeconds(f64),
    UntilSilence { max_seconds: f64, threshold_db: f32 },
}

/// Peak ceilings the dialog offers. Every entry is a real
/// `ExportNormalizeMode::PeakDb` value the engine applies verbatim — the UI
/// names the number instead of hiding one behind a generic "Normalize".
pub const PEAK_TARGETS_DB: [f32; 4] = [-0.1, -0.3, -1.0, -3.0];

/// Tail length used by [`ExportTailChoice::FixedSeconds`] when the user picks
/// the fixed option. Lives here (not in the window) so the default settings and
/// the dropdown cannot drift apart.
pub const TAIL_FIXED_SECONDS: f64 = 5.0;
/// Cap for [`ExportTailChoice::UntilSilence`].
pub const TAIL_SILENCE_MAX_SECONDS: f64 = 10.0;
/// Block-peak threshold that ends an "until silence" tail.
pub const TAIL_SILENCE_THRESHOLD_DB: f32 = -60.0;

/// Compression presets `FlacEncodeOptions::compression_level` accepts.
pub const FLAC_COMPRESSION_RANGE: (u8, u8) = (0, 8);

/// Bytes of RIFF/`fmt `/`data` header a canonical WAV file carries ahead of its
/// samples. Only used to make the size readout honest about its overhead.
const WAV_HEADER_BYTES: u64 = 44;

/// Project-derived values the settings need to build a request without reaching
/// into live timeline/engine state.
#[derive(Debug, Clone)]
pub struct ExportProjectDefaults {
    pub project_sample_rate: u32,
    /// Linear master gain to bake into the export (mirrors the engine atomic).
    pub master_volume: f32,
    /// End beat of the latest content (for the EntireArrangement estimate).
    pub content_end_beat: f64,
    pub time_selection: Option<(f64, f64)>,
    pub loop_range: Option<(f64, f64)>,
    /// Whether the build can encode MP3 (the `mp3` feature is compiled in).
    pub mp3_available: bool,
    pub track_targets: Vec<ExportTrackTarget>,
    /// The running audio device's sample rate, which a realtime render is
    /// recorded at. `0` when no device is running: realtime is unavailable.
    pub live_sample_rate: u32,
}

#[derive(Debug, Clone)]
pub struct ExportSettings {
    pub output_path: Option<PathBuf>,
    pub format: AudioFileFormat,
    pub sample_rate: ExportSampleRateChoice,
    pub channels: ExportChannelMode,
    pub range: ExportRangeChoice,
    pub wav_sample_format: AudioSampleFormat,
    pub flac_bit_depth: u16,
    pub flac_compression_level: Option<u8>,
    pub mp3_bitrate_kbps: u16,
    pub normalize: ExportNormalizeChoice,
    pub tail: ExportTailChoice,
    /// Write the stereo mixdown (the master).
    pub include_mixdown: bool,
    /// Mixer channels to write a file each for, by id. Any mix of these and
    /// the mixdown comes out of one render.
    pub selected_tracks: Vec<String>,
    pub render_mode: ExportRenderMode,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            output_path: None,
            format: AudioFileFormat::Wav,
            sample_rate: ExportSampleRateChoice::Project,
            channels: ExportChannelMode::Stereo,
            range: ExportRangeChoice::EntireArrangement,
            wav_sample_format: AudioSampleFormat::I24,
            flac_bit_depth: 24,
            flac_compression_level: Some(5),
            mp3_bitrate_kbps: 256,
            normalize: ExportNormalizeChoice::Off,
            // Capture reverb/delay/instrument-release tails past the last content
            // by default so exports don't hard-cut the decay.
            tail: ExportTailChoice::FixedSeconds(TAIL_FIXED_SECONDS),
            include_mixdown: true,
            selected_tracks: Vec::new(),
            render_mode: ExportRenderMode::Offline,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExportSettingsError {
    NoOutputPath,
    OutputDirMissing(PathBuf),
    InvalidRange,
    NoContent,
    UnsupportedSampleRate(u32),
    Mp3Unavailable,
    FlacUnsupportedBitDepth(u16),
    /// Neither the mixdown nor any channel is selected.
    NothingToRender,
    /// Realtime chosen with no audio device running.
    RealtimeUnavailable,
    /// Realtime chosen with a normalized mixdown: one pass cannot know the
    /// peak it normalizes to.
    RealtimeNormalize,
}

impl ExportSettingsError {
    /// Fluent key for the localized form of this error.
    ///
    /// This module stays GPUI- and globals-free, so it publishes the *key* and
    /// lets the window resolve it through `I18n::tr_or(key, &user_message())`.
    pub fn message_key(&self) -> &'static str {
        match self {
            Self::NoOutputPath => "export.error.no-output-path",
            Self::OutputDirMissing(_) => "export.error.output-dir-missing",
            Self::InvalidRange => "export.error.invalid-range",
            Self::NoContent => "export.error.no-content",
            Self::UnsupportedSampleRate(_) => "export.error.unsupported-sample-rate",
            Self::Mp3Unavailable => "export.error.mp3-unavailable",
            Self::FlacUnsupportedBitDepth(_) => "export.error.flac-bit-depth",
            Self::NothingToRender => "export.error.nothing-to-render",
            Self::RealtimeUnavailable => "export.error.realtime-unavailable",
            Self::RealtimeNormalize => "export.error.realtime-normalize",
        }
    }

    /// A concise, DAW-appropriate user-facing message.
    pub fn user_message(&self) -> String {
        match self {
            Self::NoOutputPath => "Choose an output file.".to_string(),
            Self::OutputDirMissing(p) => {
                format!("Output folder does not exist: {}", p.display())
            }
            Self::InvalidRange => "No valid export range selected.".to_string(),
            Self::NoContent => "The arrangement is empty — nothing to export.".to_string(),
            Self::UnsupportedSampleRate(sr) => format!("Unsupported sample rate: {sr} Hz."),
            Self::Mp3Unavailable => "MP3 export is not available in this build.".to_string(),
            Self::FlacUnsupportedBitDepth(b) => {
                format!("FLAC supports 16-bit or 24-bit, not {b}-bit.")
            }
            Self::NothingToRender => {
                "Choose the mixdown, one or more channels, or both.".to_string()
            }
            Self::RealtimeUnavailable => {
                "Realtime render needs a running audio device.".to_string()
            }
            Self::RealtimeNormalize => {
                "A realtime render is recorded in one pass and cannot be normalized.".to_string()
            }
        }
    }
}

const SUPPORTED_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];

/// Everything the dialog's readouts show, derived once from the *same*
/// [`ArrangementExportRequest`] the engine will receive.
///
/// Deriving it from the request rather than recomputing the arithmetic is what
/// keeps the summary honest: if a number here is wrong, the export is wrong the
/// same way. Building one walks the snapshot's tempo map and stats the output
/// folder, so callers cache it and refresh on mutation — never per frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportEstimate {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: AudioSampleFormat,
    pub format: AudioFileFormat,
    /// MP3 is lossy, so its "depth" is a bitrate. `None` for PCM containers.
    pub mp3_bitrate_kbps: Option<u16>,
    pub start_sample: u64,
    pub end_sample: u64,
    pub content_frames: u64,
    /// Frames the tail mode may add. `UntilSilence` reports its cap, so the
    /// real file can be shorter — readouts built from it say "up to".
    pub max_tail_frames: u64,
    pub content_seconds: f64,
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// Number of files this export will write.
    pub file_count: usize,
    /// Size of one uncompressed output file, when the container stores
    /// fixed-width PCM. `None` for FLAC/MP3 — only the encoder knows those.
    pub uncompressed_bytes: Option<u64>,
}

impl ExportSettings {
    /// Default output file name for a project, e.g. `MyProject.wav`.
    pub fn default_file_name(project_name: &str, format: AudioFileFormat) -> String {
        let stem = if project_name.trim().is_empty() {
            "Export"
        } else {
            project_name.trim()
        };
        format!("{stem}.{}", format.extension())
    }

    /// Replace the output path's extension to match the selected format.
    pub fn normalized_output_path(&self) -> Option<PathBuf> {
        self.output_path
            .as_ref()
            .map(|p| p.with_extension(self.format.extension()))
    }

    /// Resolve the chosen range to absolute `[start, end)` samples at the output
    /// sample rate, honoring the snapshot tempo map.
    fn resolve_range_samples(
        &self,
        snapshot: &EngineProjectSnapshot,
        sample_rate: u32,
    ) -> Result<(u64, u64), ExportSettingsError> {
        let beats_to = |b: f64| beats_to_samples(snapshot, b, sample_rate);
        let (start, end) = match self.range {
            ExportRangeChoice::EntireArrangement => {
                arrangement_bounds_samples(snapshot, sample_rate)
            }
            ExportRangeChoice::TimeSelection {
                start_beat,
                end_beat,
            }
            | ExportRangeChoice::LoopRange {
                start_beat,
                end_beat,
            }
            | ExportRangeChoice::Custom {
                start_beat,
                end_beat,
            } => (beats_to(start_beat), beats_to(end_beat)),
        };
        if end <= start {
            return match self.range {
                ExportRangeChoice::EntireArrangement => Err(ExportSettingsError::NoContent),
                _ => Err(ExportSettingsError::InvalidRange),
            };
        }
        Ok((start, end))
    }

    /// The rate the files are written at. A realtime render records at the
    /// device's rate, whatever the choice says.
    pub fn resolved_sample_rate(&self, defaults: &ExportProjectDefaults) -> u32 {
        match self.render_mode {
            ExportRenderMode::Realtime => defaults.live_sample_rate,
            ExportRenderMode::Offline => self.sample_rate.resolve(defaults.project_sample_rate),
        }
    }

    /// The selected channels that exist in this project, in mixer order.
    pub fn selected_targets<'a>(
        &self,
        defaults: &'a ExportProjectDefaults,
    ) -> Vec<&'a ExportTrackTarget> {
        defaults
            .track_targets
            .iter()
            .filter(|target| self.selected_tracks.iter().any(|id| *id == target.id))
            .collect()
    }

    /// How many channel files the render writes.
    pub fn batch_target_count(&self, defaults: &ExportProjectDefaults) -> usize {
        self.selected_targets(defaults).len()
    }

    /// Files the render writes in all: the mixdown and the channels.
    pub fn file_count(&self, defaults: &ExportProjectDefaults) -> usize {
        usize::from(self.include_mixdown) + self.batch_target_count(defaults)
    }

    pub fn is_track_selected(&self, id: &str) -> bool {
        self.selected_tracks.iter().any(|selected| selected == id)
    }

    /// Add or remove one channel.
    pub fn toggle_track(&mut self, id: &str) {
        if let Some(index) = self
            .selected_tracks
            .iter()
            .position(|selected| selected == id)
        {
            self.selected_tracks.remove(index);
        } else {
            self.selected_tracks.push(id.to_string());
        }
    }

    /// Replace the channel selection with a quick pick.
    pub fn apply_channel_preset(
        &mut self,
        preset: ExportChannelPreset,
        defaults: &ExportProjectDefaults,
    ) {
        self.selected_tracks = defaults
            .track_targets
            .iter()
            .filter(|target| match preset {
                ExportChannelPreset::All => true,
                ExportChannelPreset::SourceTracks => target.include_in_multitrack,
                ExportChannelPreset::None => false,
            })
            .map(|target| target.id.clone())
            .collect();
    }

    fn sample_format(&self) -> AudioSampleFormat {
        match self.format {
            AudioFileFormat::Wav => self.wav_sample_format,
            AudioFileFormat::Flac => {
                if self.flac_bit_depth == 16 {
                    AudioSampleFormat::I16
                } else {
                    AudioSampleFormat::I24
                }
            }
            // MP3 is lossy; sample format is informational only.
            AudioFileFormat::Mp3 => AudioSampleFormat::I16,
            AudioFileFormat::Rauf => AudioSampleFormat::F32,
        }
    }

    /// Validate the settings against project defaults. Does not require a
    /// snapshot (range-content checks happen in [`to_request`]).
    pub fn validate(&self, defaults: &ExportProjectDefaults) -> Result<(), ExportSettingsError> {
        let path = self
            .normalized_output_path()
            .ok_or(ExportSettingsError::NoOutputPath)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(ExportSettingsError::OutputDirMissing(parent.to_path_buf()));
            }
        }
        let realtime = self.render_mode == ExportRenderMode::Realtime;
        if realtime && defaults.live_sample_rate == 0 {
            return Err(ExportSettingsError::RealtimeUnavailable);
        }
        let sr = self.resolved_sample_rate(defaults);
        // WAV writes any rate, so a realtime render at an unusual device rate
        // still has a format that takes it.
        if !SUPPORTED_RATES.contains(&sr) && !(realtime && self.format == AudioFileFormat::Wav) {
            return Err(ExportSettingsError::UnsupportedSampleRate(sr));
        }
        if self.format == AudioFileFormat::Mp3 && !defaults.mp3_available {
            return Err(ExportSettingsError::Mp3Unavailable);
        }
        if self.format == AudioFileFormat::Flac
            && self.flac_bit_depth != 16
            && self.flac_bit_depth != 24
        {
            return Err(ExportSettingsError::FlacUnsupportedBitDepth(
                self.flac_bit_depth,
            ));
        }
        if self.file_count(defaults) == 0 {
            return Err(ExportSettingsError::NothingToRender);
        }
        if realtime && self.include_mixdown && self.normalize != ExportNormalizeChoice::Off {
            return Err(ExportSettingsError::RealtimeNormalize);
        }
        Ok(())
    }

    /// Resolve the readouts the dialog shows, from the request the engine gets.
    pub fn estimate(
        &self,
        snapshot: &EngineProjectSnapshot,
        defaults: &ExportProjectDefaults,
    ) -> Result<ExportEstimate, ExportSettingsError> {
        let request = self.to_request(snapshot, defaults)?;
        let sample_rate = request.render.sample_rate;
        let rate = sample_rate.max(1) as f64;
        let content_frames = request.render.content_frames();
        let max_tail_frames = request.render.max_tail_frames();
        let file_count = self.file_count(defaults);
        let uncompressed_bytes = (request.format == AudioFileFormat::Wav).then(|| {
            content_frames
                .saturating_add(max_tail_frames)
                .saturating_mul(request.render.channels as u64)
                .saturating_mul(request.sample_format.bytes_per_sample() as u64)
                .saturating_add(WAV_HEADER_BYTES)
        });
        Ok(ExportEstimate {
            sample_rate,
            channels: request.render.channels,
            sample_format: request.sample_format,
            format: request.format,
            mp3_bitrate_kbps: (request.format == AudioFileFormat::Mp3)
                .then_some(self.mp3_bitrate_kbps),
            start_sample: request.render.start_sample,
            end_sample: request.render.end_sample,
            content_frames,
            max_tail_frames,
            content_seconds: content_frames as f64 / rate,
            start_seconds: request.render.start_sample as f64 / rate,
            end_seconds: request.render.end_sample as f64 / rate,
            file_count,
            uncompressed_bytes,
        })
    }

    /// Build the whole render: the mixdown to `output_path` when it is
    /// included, and one file per selected channel in `<folder>/<base>
    /// Stems/`, numbered in mixer order. Channels are never normalized — they
    /// would no longer sum to the mix.
    pub fn to_job(
        &self,
        snapshot: &EngineProjectSnapshot,
        defaults: &ExportProjectDefaults,
        base_name: &str,
    ) -> Result<RenderJob, ExportSettingsError> {
        let request = self.to_request(snapshot, defaults)?;
        let folder = request
            .output_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("{} Stems", sanitize_file_stem(base_name)));
        let tracks = self
            .selected_targets(defaults)
            .into_iter()
            .enumerate()
            .map(|(index, target)| {
                let mut track_request = request.clone();
                track_request.render.normalize = ExportNormalizeMode::None;
                track_request.output_path = folder.join(format!(
                    "{:02} {}.{}",
                    index + 1,
                    sanitize_file_stem(&target.name),
                    request.format.extension()
                ));
                TrackExportTarget {
                    track_id: target.id.clone(),
                    request: track_request,
                }
            })
            .collect();
        Ok(RenderJob {
            mixdown: self.include_mixdown.then_some(request),
            tracks,
        })
    }

    /// Build the engine [`ArrangementExportRequest`]. Validates first, then
    /// resolves the range against the snapshot tempo map.
    pub fn to_request(
        &self,
        snapshot: &EngineProjectSnapshot,
        defaults: &ExportProjectDefaults,
    ) -> Result<ArrangementExportRequest, ExportSettingsError> {
        self.validate(defaults)?;
        let output_path = self
            .normalized_output_path()
            .ok_or(ExportSettingsError::NoOutputPath)?;
        let sample_rate = self.resolved_sample_rate(defaults);
        let (start_sample, end_sample) = self.resolve_range_samples(snapshot, sample_rate)?;

        let tail = match self.tail {
            ExportTailChoice::None => ExportTailMode::None,
            ExportTailChoice::FixedSeconds(s) => ExportTailMode::FixedSeconds(s),
            ExportTailChoice::UntilSilence {
                max_seconds,
                threshold_db,
            } => ExportTailMode::UntilSilence {
                max_seconds,
                threshold_db,
            },
        };
        let normalize = match self.normalize {
            ExportNormalizeChoice::Off => ExportNormalizeMode::None,
            ExportNormalizeChoice::PeakDb(db) => ExportNormalizeMode::PeakDb(db),
        };

        let mut encode_options = AudioEncodeOptions {
            format: self.format,
            ..Default::default()
        };
        encode_options.flac = FlacEncodeOptions {
            compression_level: self.flac_compression_level.unwrap_or(5),
            block_size: 4096,
        };
        encode_options.mp3 = Mp3EncodeOptions {
            bitrate: Mp3Bitrate::from_kbps(self.mp3_bitrate_kbps as u32)
                .unwrap_or(Mp3Bitrate::Kbps256),
            quality: 2,
        };

        Ok(ArrangementExportRequest {
            output_path,
            format: self.format,
            sample_format: self.sample_format(),
            render: OfflineRenderRequest {
                sample_rate,
                channels: self.channels.channels(),
                start_sample,
                end_sample,
                master_volume: defaults.master_volume,
                block_size: 1024,
                tail,
                normalize,
            },
            encode_options,
        })
    }
}

/// A file name from a user or track name: characters no file system takes
/// become `_`, and nothing at all becomes "Track".
pub fn sanitize_file_stem(name: &str) -> String {
    let sanitized: String = name
        .trim()
        .chars()
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect();
    if sanitized.is_empty() {
        "Track".to_string()
    } else {
        sanitized
    }
}
