//! Headerless ("raw") PCM import.
//!
//! A `.raw` / `.pcm` file carries no sample rate, channel count or sample
//! format, so it cannot be opened like any other audio file: the user says how
//! it is laid out (the "Import Raw PCM" dialog), and the file is converted once
//! — decoded by MKFF with exactly that layout — into a 32-bit float WAV in the
//! project's `Assets/Audio` folder. The arrangement, the engine and project
//! save then only ever see that WAV, so nothing downstream needs to know the
//! source was headerless.

use std::path::{Path, PathBuf};

use mkff_audio::format::{PcmFormat, WavSampleFormat, WavWriter};
use mkff_audio::{AudioFile, RawPcmSpec};

/// `true` for the extensions Studio treats as headerless PCM.
pub fn is_raw_pcm_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| mkff_audio::RAW_PCM_EXTENSIONS.contains(&ext.as_str()))
}

/// How the samples of a headerless file are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RawSampleFormat {
    U8,
    S8,
    S16,
    S24,
    S32,
    F32,
    F64,
    ALaw,
    MuLaw,
}

impl RawSampleFormat {
    pub const ALL: [RawSampleFormat; 9] = [
        Self::U8,
        Self::S8,
        Self::S16,
        Self::S24,
        Self::S32,
        Self::F32,
        Self::F64,
        Self::ALaw,
        Self::MuLaw,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::U8 => "U8",
            Self::S8 => "S8",
            Self::S16 => "S16",
            Self::S24 => "S24",
            Self::S32 => "S32",
            Self::F32 => "F32",
            Self::F64 => "F64",
            Self::ALaw => "A-law",
            Self::MuLaw => "µ-law",
        }
    }

    pub fn bytes_per_sample(self) -> u64 {
        match self {
            Self::U8 | Self::S8 | Self::ALaw | Self::MuLaw => 1,
            Self::S16 => 2,
            Self::S24 => 3,
            Self::S32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// Byte order only means something for multi-byte samples.
    pub fn has_byte_order(self) -> bool {
        self.bytes_per_sample() > 1
    }

    fn pcm(self, big_endian: bool) -> PcmFormat {
        match self {
            Self::U8 => PcmFormat::u8(),
            Self::S8 => PcmFormat::int(8, false),
            Self::S16 => PcmFormat::int(16, big_endian),
            Self::S24 => PcmFormat::int(24, big_endian),
            Self::S32 => PcmFormat::int(32, big_endian),
            Self::F32 => PcmFormat::float(32, big_endian),
            Self::F64 => PcmFormat::float(64, big_endian),
            Self::ALaw => PcmFormat::alaw(),
            Self::MuLaw => PcmFormat::mulaw(),
        }
    }
}

/// Sample rates offered in the dialog.
pub const RAW_SAMPLE_RATES: [u32; 9] = [
    8_000, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 192_000,
];

/// Channel counts offered in the dialog.
pub const RAW_CHANNEL_COUNTS: [u32; 5] = [1, 2, 4, 6, 8];

/// The layout the user gives a headerless file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawPcmSettings {
    pub sample_rate: u32,
    pub channels: u32,
    pub format: RawSampleFormat,
    pub big_endian: bool,
}

impl Default for RawPcmSettings {
    /// CD-style 16-bit stereo little-endian at 44.1 kHz: what most raw dumps
    /// (and every `.pcm` from a WAV body) are.
    fn default() -> Self {
        Self {
            sample_rate: 44_100,
            channels: 2,
            format: RawSampleFormat::S16,
            big_endian: false,
        }
    }
}

impl RawPcmSettings {
    pub fn bytes_per_frame(&self) -> u64 {
        self.format.bytes_per_sample() * u64::from(self.channels.max(1))
    }

    /// Whole frames in a file of `file_len` bytes, and the trailing bytes that
    /// do not make a whole frame (ignored on import).
    pub fn frames_in(&self, file_len: u64) -> (u64, u64) {
        let frame = self.bytes_per_frame().max(1);
        (file_len / frame, file_len % frame)
    }

    pub fn duration_seconds(&self, file_len: u64) -> f64 {
        let (frames, _) = self.frames_in(file_len);
        frames as f64 / f64::from(self.sample_rate.max(1))
    }

    pub fn spec(&self) -> RawPcmSpec {
        RawPcmSpec {
            sample_rate: self.sample_rate,
            channels: self.channels,
            format: self
                .format
                .pcm(self.big_endian && self.format.has_byte_order()),
            data_offset: 0,
        }
    }
}

/// `m:ss.t` for a duration line.
pub fn format_duration(seconds: f64) -> String {
    let tenths = (seconds.max(0.0) * 10.0).round() as u64;
    let (minutes, rest) = (tenths / 600, tenths % 600);
    format!("{minutes}:{:02}.{}", rest / 10, rest % 10)
}

/// Where a converted file goes: the project's `Assets/Audio` folder, or the
/// user Recordings folder before the project has one. Project save copies
/// audio from outside the project folder in, like any other external file.
pub fn conversion_folder(project_folder: Option<&Path>) -> PathBuf {
    match project_folder {
        Some(root) => crate::paths::ProjectFolderLayout::from_root(root.to_path_buf()).media_audio,
        None => crate::paths::FutureboardPaths::resolve().recordings,
    }
}

/// `<folder>/<stem>.wav`, or `<stem>-2.wav`, `<stem>-3.wav`, … when taken.
fn unique_wav_path(folder: &Path, source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Raw PCM");
    let first = folder.join(format!("{stem}.wav"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| folder.join(format!("{stem}-{n}.wav")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

/// Decode `source` with `settings` and write it as a 32-bit float WAV into
/// `folder`. Blocking file I/O: call it off the UI thread. Returns the WAV.
pub fn convert_raw_pcm(
    source: &Path,
    settings: RawPcmSettings,
    folder: &Path,
) -> Result<PathBuf, String> {
    let mut audio = AudioFile::open_raw_pcm(source, settings.spec())
        .map_err(|e| format!("Could not read {} as raw PCM: {e}", source.display()))?;
    if audio.duration() == Some(0) {
        return Err(format!(
            "{} holds less than one frame at these settings",
            source.display()
        ));
    }
    std::fs::create_dir_all(folder)
        .map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
    let target = unique_wav_path(folder, source);
    let partial = target.with_extension("wav.partial");

    let result = (|| -> Result<(), String> {
        let file = std::fs::File::create(&partial).map_err(|e| e.to_string())?;
        let mut writer = WavWriter::new(
            std::io::BufWriter::new(file),
            settings.sample_rate,
            settings.channels,
            WavSampleFormat::Float32,
        )
        .map_err(|e| e.to_string())?;
        while let Some(chunk) = audio.next_chunk().map_err(|e| e.to_string())? {
            writer.write(&chunk).map_err(|e| e.to_string())?;
        }
        let mut out = writer.finish().map_err(|e| e.to_string())?;
        std::io::Write::flush(&mut out).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("Converting {} failed: {e}", source.display()));
    }
    std::fs::rename(&partial, &target).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!("Could not finish {}: {e}", target.display())
    })?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fb-raw-pcm-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn raw_extensions_are_recognised_case_insensitively() {
        assert!(is_raw_pcm_path(Path::new("C:/dumps/take.RAW")));
        assert!(is_raw_pcm_path(Path::new("/tmp/voice.pcm")));
        assert!(!is_raw_pcm_path(Path::new("/tmp/voice.wav")));
    }

    #[test]
    fn frame_math_ignores_a_partial_trailing_frame() {
        let settings = RawPcmSettings::default(); // s16 stereo: 4 bytes/frame
        assert_eq!(settings.bytes_per_frame(), 4);
        assert_eq!(settings.frames_in(4 * 44_100 + 3), (44_100, 3));
        assert!((settings.duration_seconds(4 * 44_100) - 1.0).abs() < 1e-9);
        assert_eq!(format_duration(61.25), "1:01.3");
    }

    #[test]
    fn byte_order_is_dropped_for_single_byte_formats() {
        let settings = RawPcmSettings {
            format: RawSampleFormat::MuLaw,
            big_endian: true,
            ..RawPcmSettings::default()
        };
        assert_eq!(settings.spec().format, PcmFormat::mulaw());
    }

    #[test]
    fn converts_big_endian_s24_mono_to_a_float_wav_with_the_same_samples() {
        let dir = temp_dir("s24be");
        let source = dir.join("dump.raw");
        let samples: [i32; 4] = [0, 4_194_304, -8_388_608, 8_388_607];
        let mut bytes = Vec::new();
        for s in samples {
            bytes.extend_from_slice(&s.to_be_bytes()[1..]);
        }
        bytes.push(0xAA); // a partial trailing frame
        std::fs::write(&source, &bytes).unwrap();

        let settings = RawPcmSettings {
            sample_rate: 48_000,
            channels: 1,
            format: RawSampleFormat::S24,
            big_endian: true,
        };
        let out_dir = dir.join("Assets").join("Audio");
        let wav = convert_raw_pcm(&source, settings, &out_dir).unwrap();
        assert_eq!(wav, out_dir.join("dump.wav"));

        let mut decoded = AudioFile::open(&wav).unwrap();
        assert_eq!(decoded.sample_rate(), 48_000);
        assert_eq!(decoded.channels(), 1);
        let buffer = decoded.read_to_end().unwrap();
        let expected = [0.0f32, 0.5, -1.0, 8_388_607.0 / 8_388_608.0];
        assert_eq!(buffer.channels[0].len(), expected.len());
        for (got, want) in buffer.channels[0].iter().zip(expected) {
            assert!((got - want).abs() < 1e-7, "{got} vs {want}");
        }

        // A second import of the same name does not overwrite the first.
        let again = convert_raw_pcm(&source, settings, &out_dir).unwrap();
        assert_eq!(again, out_dir.join("dump-2.wav"));
        assert!(!out_dir.join("dump.wav.partial").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_shorter_than_one_frame_is_refused() {
        let dir = temp_dir("short");
        let source = dir.join("tiny.pcm");
        std::fs::write(&source, [1u8, 2, 3]).unwrap();
        let err = convert_raw_pcm(&source, RawPcmSettings::default(), &dir).unwrap_err();
        assert!(err.contains("tiny.pcm"), "{err}");
        assert!(std::fs::read_dir(&dir).unwrap().count() == 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
