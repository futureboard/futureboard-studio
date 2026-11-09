//! MP3 (CBR) encoding via `mp3lame-encoder`, which builds libmp3lame (LAME)
//! from source through `mp3lame-sys`. This is NOT FFmpeg/libav and spawns no
//! external process. The whole path is gated behind the `mp3` feature because
//! libmp3lame requires a C toolchain to build.
//!
//! Streaming: frames are converted to interleaved/mono `i16` and pushed through
//! LAME block by block, appended straight to the output file. The Xing/Info VBR
//! tag is disabled so the CBR stream is valid without seeking back to the start.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use mp3lame_encoder::{
    Bitrate, Builder, FlushNoGap, Id3Tag, InterleavedPcm, MonoPcm, Quality,
    max_required_buffer_size,
};

use crate::format::{
    AudioDither, AudioEncodeSpec, AudioEncodeSummary, AudioEncoder, AudioFileFormat, EncodeError,
    Mp3Bitrate, Quantizer, check_interleaved_len,
};
use crate::metadata::AudioMetadata;

pub struct Mp3Encoder {
    encoder: mp3lame_encoder::Encoder,
    writer: BufWriter<File>,
    spec: AudioEncodeSpec,
    frames_written: u64,
    bytes_written: u64,
    finalized: bool,
    /// Reused i16 conversion scratch to avoid per-block allocation.
    pcm: Vec<i16>,
    /// Reused encoded-bytes scratch.
    out: Vec<u8>,
    /// Float/i32 to the 16-bit PCM LAME takes; [`AudioDither::Off`] unless
    /// set by [`with_dither`](Mp3Encoder::with_dither).
    quantizer: Quantizer,
}

fn map_bitrate(b: Mp3Bitrate) -> Bitrate {
    match b {
        Mp3Bitrate::Kbps128 => Bitrate::Kbps128,
        Mp3Bitrate::Kbps192 => Bitrate::Kbps192,
        Mp3Bitrate::Kbps256 => Bitrate::Kbps256,
        Mp3Bitrate::Kbps320 => Bitrate::Kbps320,
    }
}

fn map_quality(q: u8) -> Quality {
    match q {
        0 => Quality::Best,
        1 => Quality::SecondBest,
        2 => Quality::NearBest,
        3 => Quality::VeryNice,
        4 => Quality::Nice,
        5 => Quality::Good,
        6 => Quality::Decent,
        7 => Quality::Ok,
        8 => Quality::SecondWorst,
        _ => Quality::Worst,
    }
}

impl Mp3Encoder {
    pub fn create(
        path: impl AsRef<Path>,
        spec: AudioEncodeSpec,
        options: crate::format::Mp3EncodeOptions,
        metadata: AudioMetadata,
    ) -> Result<Self, EncodeError> {
        spec.validate()?;
        if spec.channels != 1 && spec.channels != 2 {
            return Err(EncodeError::UnsupportedSpec(format!(
                "MP3 export supports mono or stereo only, got {} channels",
                spec.channels
            )));
        }
        if spec.sample_rate != 44_100 && spec.sample_rate != 48_000 {
            return Err(EncodeError::UnsupportedSpec(format!(
                "MP3 export supports 44100 or 48000 Hz only, got {}",
                spec.sample_rate
            )));
        }

        let mut builder = Builder::new()
            .ok_or_else(|| EncodeError::FinalizeFailed("failed to allocate LAME builder".into()))?;
        let build_err = |e: mp3lame_encoder::BuildError| {
            EncodeError::UnsupportedSpec(format!("LAME config rejected: {e:?}"))
        };
        builder
            .set_num_channels(spec.channels as u8)
            .map_err(build_err)?;
        builder
            .set_sample_rate(spec.sample_rate)
            .map_err(build_err)?;
        builder
            .set_brate(map_bitrate(options.bitrate))
            .map_err(build_err)?;
        builder
            .set_quality(map_quality(options.quality))
            .map_err(build_err)?;
        // Keep the stream seekless: no Xing/Info VBR tag at offset 0.
        builder.set_to_write_vbr_tag(false).map_err(build_err)?;

        // Minimal ID3v2 tag (best-effort). Byte buffers must outlive build().
        let title = metadata.title.clone().unwrap_or_default().into_bytes();
        let artist = metadata.artist.clone().unwrap_or_default().into_bytes();
        let album = metadata.album.clone().unwrap_or_default().into_bytes();
        let year = metadata.date.clone().unwrap_or_default().into_bytes();
        let comment = metadata.comment.clone().unwrap_or_default().into_bytes();
        let tag = Id3Tag {
            title: &title,
            artist: &artist,
            album: &album,
            album_art: &[],
            year: &year,
            comment: &comment,
        };
        if tag.is_any_set()
            && let Err(e) = builder.set_id3_tag(tag)
        {
            tracing::warn!("MP3 ID3 tag skipped: {e:?}");
        }

        let encoder = builder
            .build()
            .map_err(|e| EncodeError::FinalizeFailed(format!("LAME build failed: {e:?}")))?;

        let writer = BufWriter::new(File::create(path)?);
        Ok(Self {
            encoder,
            writer,
            spec,
            frames_written: 0,
            bytes_written: 0,
            finalized: false,
            pcm: Vec::new(),
            out: Vec::new(),
            quantizer: Quantizer::new(AudioDither::Off),
        })
    }

    /// Dither the 16-bit PCM handed to LAME with `dither`.
    pub fn with_dither(mut self, dither: AudioDither) -> Self {
        self.quantizer = Quantizer::new(dither);
        self
    }

    fn encode_pcm(&mut self) -> Result<(), EncodeError> {
        let channels = self.spec.channels as usize;
        let frames = self.pcm.len() / channels;
        if frames == 0 {
            return Ok(());
        }
        self.out.clear();
        self.out.reserve(max_required_buffer_size(frames));
        let written = if channels == 1 {
            self.encoder
                .encode_to_vec(MonoPcm(&self.pcm), &mut self.out)
        } else {
            self.encoder
                .encode_to_vec(InterleavedPcm(&self.pcm), &mut self.out)
        }
        .map_err(|e| EncodeError::FinalizeFailed(format!("LAME encode failed: {e:?}")))?;
        self.writer.write_all(&self.out[..written])?;
        self.bytes_written += written as u64;
        self.frames_written += frames as u64;
        Ok(())
    }
}

impl AudioEncoder for Mp3Encoder {
    fn format(&self) -> AudioFileFormat {
        AudioFileFormat::Mp3
    }

    fn spec(&self) -> AudioEncodeSpec {
        self.spec.clone()
    }

    fn write_interleaved_f32(&mut self, frames: &[f32]) -> Result<(), EncodeError> {
        if self.finalized {
            return Err(EncodeError::InvalidInput("write after finalize".into()));
        }
        check_interleaved_len(frames.len(), self.spec.channels)?;
        self.pcm.clear();
        self.pcm.reserve(frames.len());
        for &x in frames {
            self.pcm.push(self.quantizer.f32_to_i16(x));
        }
        self.encode_pcm()
    }

    fn write_interleaved_i32(&mut self, frames: &[i32]) -> Result<(), EncodeError> {
        if self.finalized {
            return Err(EncodeError::InvalidInput("write after finalize".into()));
        }
        check_interleaved_len(frames.len(), self.spec.channels)?;
        self.pcm.clear();
        self.pcm.reserve(frames.len());
        for &x in frames {
            self.pcm.push(self.quantizer.i32_to_i16(x));
        }
        self.encode_pcm()
    }

    fn finalize(&mut self) -> Result<AudioEncodeSummary, EncodeError> {
        if self.finalized {
            return Err(EncodeError::FinalizeFailed(
                "encoder already finalized".into(),
            ));
        }
        self.finalized = true;

        self.out.clear();
        self.out.reserve(7200);
        let written = self
            .encoder
            .flush_to_vec::<FlushNoGap>(&mut self.out)
            .map_err(|e| EncodeError::FinalizeFailed(format!("LAME flush failed: {e:?}")))?;
        self.writer.write_all(&self.out[..written])?;
        self.bytes_written += written as u64;
        self.writer.flush()?;
        self.writer.get_mut().sync_all()?;

        Ok(AudioEncodeSummary {
            format: AudioFileFormat::Mp3,
            sample_rate: self.spec.sample_rate,
            channels: self.spec.channels,
            sample_format: self.spec.sample_format,
            frames_written: self.frames_written,
            bytes_written: self.bytes_written,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{AudioSampleFormat, Mp3Bitrate, Mp3EncodeOptions};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "futureboard-{name}-{}-{}.mp3",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    fn spec(rate: u32, ch: u16) -> AudioEncodeSpec {
        AudioEncodeSpec {
            sample_rate: rate,
            channels: ch,
            sample_format: AudioSampleFormat::I16,
        }
    }

    #[test]
    fn encodes_stereo_44k_nonempty_with_mp3_sync() {
        let path = temp_path("stereo");
        let mut enc = Mp3Encoder::create(
            &path,
            spec(44_100, 2),
            Mp3EncodeOptions {
                bitrate: Mp3Bitrate::Kbps192,
                quality: 5,
            },
            AudioMetadata::default(),
        )
        .unwrap();
        let mut block = Vec::new();
        for n in 0..44_100u32 {
            let t = n as f32 / 44_100.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            block.push(s);
            block.push(s);
        }
        enc.write_interleaved_f32(&block).unwrap();
        let summary = enc.finalize().unwrap();
        assert!(summary.bytes_written > 0);

        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.is_empty());
        // First MP3 audio frame starts with an MPEG sync word (0xFFE_).
        assert_eq!(bytes[0], 0xFF, "missing MPEG frame sync");
        assert_eq!(bytes[1] & 0xE0, 0xE0, "missing MPEG frame sync bits");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_unsupported_sample_rate() {
        let path = temp_path("badrate");
        let result = Mp3Encoder::create(
            &path,
            spec(96_000, 2),
            Mp3EncodeOptions::default(),
            AudioMetadata::default(),
        );
        assert!(matches!(result, Err(EncodeError::UnsupportedSpec(_))));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bitrate_option_validation() {
        assert!(Mp3Bitrate::from_kbps(192).is_ok());
        assert!(Mp3Bitrate::from_kbps(123).is_err());
    }
}
