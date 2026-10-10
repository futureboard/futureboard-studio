//! Recording: every armed strip to its own file, written off the audio thread.
//!
//! The audio thread only pushes samples into each strip's [`RecordStream`]
//! ring. A writer thread drains them into encoders (WAV or FLAC), so a slow
//! disk shows up as counted drops in a ring — never as a glitch in the mix.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sphere_encoder::{
    AudioEncodeOptions, AudioEncodeSpec, AudioEncoder, AudioFileFormat, AudioSampleFormat,
    create_encoder,
};

use crate::graph::RecordStream;
use crate::session::{RecordFormat, RecordSettings};

/// Seconds of audio each ring holds while the disk catches up.
const RING_SECONDS: usize = 4;

/// A recording in progress.
pub struct Recording {
    pub folder: PathBuf,
    pub tracks: Vec<(String, PathBuf, Arc<RecordStream>)>,
    started: Instant,
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<Vec<String>>>,
}

/// What a finished recording left behind.
#[derive(Debug, Clone)]
pub struct RecordingSummary {
    pub folder: PathBuf,
    pub files: Vec<PathBuf>,
    pub seconds: f64,
    /// Samples lost because the disk fell behind, over all files.
    pub dropped_samples: u64,
    pub errors: Vec<String>,
}

/// A file name that is safe on every platform. Playback matches a take's
/// files to channels by it.
pub(crate) fn file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        "Track".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A folder name for a take started now: `Take 2026-10-06 21-04-33`.
fn take_folder_name() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil date from days since the epoch (UTC).
    let days = (secs / 86_400) as i64;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("Take {year:04}-{month:02}-{day:02} {h:02}-{m:02}-{s:02} UTC")
}

impl Recording {
    /// Rings for each `(name, channels)` strip, ready to be placed in the
    /// graph; nothing is written until [`Self::start`].
    pub fn prepare(
        settings: &RecordSettings,
        sample_rate: u32,
        strips: &[(String, usize)],
    ) -> Result<Self, String> {
        let folder = settings.resolved_folder().join(take_folder_name());
        std::fs::create_dir_all(&folder)
            .map_err(|error| format!("{}: {error}", folder.display()))?;
        let extension = match settings.format {
            RecordFormat::Wav => "wav",
            RecordFormat::Flac => "flac",
        };
        let mut used = std::collections::HashSet::new();
        let tracks = strips
            .iter()
            .map(|(name, channels)| {
                let stem = file_stem(name);
                let mut unique = stem.clone();
                let mut n = 2;
                while !used.insert(unique.to_lowercase()) {
                    unique = format!("{stem} {n}");
                    n += 1;
                }
                (
                    name.clone(),
                    folder.join(format!("{unique}.{extension}")),
                    Arc::new(RecordStream::new(*channels, RING_SECONDS, sample_rate)),
                )
            })
            .collect();
        Ok(Self {
            folder,
            tracks,
            started: Instant::now(),
            stop: Arc::new(AtomicBool::new(false)),
            writer: None,
        })
    }

    /// Open the files and start writing. Call once the rings are in the
    /// graph.
    pub fn start(&mut self, settings: &RecordSettings, sample_rate: u32) -> Result<(), String> {
        let mut encoders: Vec<(Arc<RecordStream>, Box<dyn AudioEncoder>, PathBuf)> = Vec::new();
        for (_, path, stream) in &self.tracks {
            let encoder = open_encoder(path, settings, sample_rate, stream.channels)?;
            encoders.push((stream.clone(), encoder, path.clone()));
        }
        let stop = self.stop.clone();
        self.started = Instant::now();
        self.writer = Some(
            std::thread::Builder::new()
                .name("livestage-recorder".into())
                .spawn(move || write_loop(encoders, stop))
                .map_err(|error| error.to_string())?,
        );
        Ok(())
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn dropped_samples(&self) -> u64 {
        self.tracks
            .iter()
            .map(|(_, _, stream)| stream.dropped.load(Ordering::Relaxed))
            .sum()
    }

    /// Stop writing, flush what the rings still hold, and close the files.
    /// Call after the rings have left the graph.
    pub fn finish(mut self) -> RecordingSummary {
        let seconds = self.started.elapsed().as_secs_f64();
        self.stop.store(true, Ordering::Release);
        let errors = self
            .writer
            .take()
            .map(|writer| {
                writer
                    .join()
                    .unwrap_or_else(|_| vec!["the recorder thread panicked".to_string()])
            })
            .unwrap_or_default();
        RecordingSummary {
            folder: self.folder.clone(),
            files: self
                .tracks
                .iter()
                .map(|(_, path, _)| path.clone())
                .collect(),
            seconds,
            dropped_samples: self.dropped_samples(),
            errors,
        }
    }
}

fn open_encoder(
    path: &Path,
    settings: &RecordSettings,
    sample_rate: u32,
    channels: usize,
) -> Result<Box<dyn AudioEncoder>, String> {
    let (format, sample_format) = match (settings.format, settings.bit_depth) {
        (RecordFormat::Wav, 16) => (AudioFileFormat::Wav, AudioSampleFormat::I16),
        (RecordFormat::Wav, 32) => (AudioFileFormat::Wav, AudioSampleFormat::F32),
        (RecordFormat::Wav, _) => (AudioFileFormat::Wav, AudioSampleFormat::I24),
        // FLAC has no float samples: 32 records at 24.
        (RecordFormat::Flac, 16) => (AudioFileFormat::Flac, AudioSampleFormat::I16),
        (RecordFormat::Flac, _) => (AudioFileFormat::Flac, AudioSampleFormat::I24),
    };
    create_encoder(
        path,
        AudioEncodeSpec {
            sample_rate,
            channels: channels as u16,
            sample_format,
        },
        AudioEncodeOptions {
            format,
            ..AudioEncodeOptions::default()
        },
    )
    .map_err(|error| format!("{}: {error}", path.display()))
}

fn write_loop(
    mut encoders: Vec<(Arc<RecordStream>, Box<dyn AudioEncoder>, PathBuf)>,
    stop: Arc<AtomicBool>,
) -> Vec<String> {
    let mut buffer = vec![0.0f32; 16_384];
    let mut errors = Vec::new();
    let mut failed = vec![false; encoders.len()];
    loop {
        let stopping = stop.load(Ordering::Acquire);
        let mut moved = false;
        for (index, (stream, encoder, path)) in encoders.iter_mut().enumerate() {
            // Whole frames only, so a pop never splits left from right.
            let usable = buffer.len() / stream.channels * stream.channels;
            loop {
                let n = stream.ring.pop(&mut buffer[..usable]);
                if n == 0 {
                    break;
                }
                moved = true;
                if failed[index] {
                    continue;
                }
                if let Err(error) = encoder.write_interleaved_f32(&buffer[..n]) {
                    failed[index] = true;
                    errors.push(format!("{}: {error}", path.display()));
                }
            }
        }
        if stopping && !moved {
            break;
        }
        if !moved {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    for ((_, encoder, path), failed) in encoders.iter_mut().zip(failed) {
        if failed {
            continue;
        }
        if let Err(error) = encoder.finalize() {
            errors.push(format!("{}: {error}", path.display()));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_writes_one_file_per_strip() {
        let dir = std::env::temp_dir().join(format!("livestage-rec-test-{}", std::process::id()));
        let settings = RecordSettings {
            folder: Some(dir.clone()),
            ..RecordSettings::default()
        };
        let mut recording = Recording::prepare(
            &settings,
            48_000,
            &[
                ("Kick".to_string(), 1),
                ("Kick".to_string(), 2),
                ("a/b".to_string(), 2),
            ],
        )
        .unwrap();
        recording.start(&settings, 48_000).unwrap();
        for (_, _, stream) in &recording.tracks {
            let samples = vec![0.25f32; 4800 * stream.channels];
            assert_eq!(stream.ring.push(&samples), samples.len());
        }
        let summary = recording.finish();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        let names: Vec<String> = summary
            .files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["Kick.wav", "Kick 2.wav", "a_b.wav"]);
        for file in &summary.files {
            let bytes = std::fs::metadata(file).unwrap().len();
            assert!(bytes > 4800 * 3, "{} is only {bytes} bytes", file.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
