//! [`AudioWriter`]: encode planar f32 straight into a file.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use mkff_audio_core::{AudioBuffer, CodecId, Encoder, Error, Packet, Result};
use mkff_audio_format::{
    AdtsWriter, FlacWriter, FlacWriterOptions, M4aTrack, M4aWriter, MpaGapless, MpaInfoTag,
    MpaWriter, WavSampleFormat, WavWriter,
};

use crate::codecs::{self, EncoderSettings};

type Out = BufWriter<File>;

/// Output container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Container {
    /// `.wav` (PCM).
    Wav,
    /// `.mp3` / `.mp2`: raw MPEG audio frames with an Info / LAME tag for gapless playback.
    Mpa,
    /// `.aac`: ADTS.
    Adts,
    /// `.flac`.
    Flac,
    /// `.m4a` / `.mp4`: AAC, ALAC, FLAC, MP3 or PCM in MP4.
    M4a,
}

impl Container {
    /// From a file extension.
    pub fn from_path(path: &Path) -> Option<Container> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "wav" | "wave" => Container::Wav,
            "mp3" | "mp2" => Container::Mpa,
            "aac" | "adts" => Container::Adts,
            "flac" => Container::Flac,
            "m4a" | "mp4" | "m4b" => Container::M4a,
            _ => return None,
        })
    }

    /// The codec a container is written with when none is named.
    pub fn default_codec(self) -> CodecId {
        match self {
            Container::Wav => CodecId::Pcm,
            Container::Mpa => CodecId::MpegAudio,
            Container::Adts | Container::M4a => CodecId::Aac,
            Container::Flac => CodecId::Flac,
        }
    }
}

enum Sink {
    Wav(WavWriter<Out>),
    Mpa(MpaWriter<Out>),
    Adts(AdtsWriter<Out>),
    Flac(FlacWriter<Out>),
    M4a(M4aWriter<Out>, usize),
}

/// Encodes audio into a file.
///
/// ```no_run
/// use mkff_audio::{AudioWriter, CodecId, EncoderSettings};
///
/// let mut w = AudioWriter::create("out.mp3", EncoderSettings::new(CodecId::MpegAudio, 44_100, 2).with_bitrate(192_000))?;
/// let left = vec![0.0f32; 44_100];
/// let right = vec![0.0f32; 44_100];
/// w.write(&[&left, &right])?;
/// w.finish()?;
/// # Ok::<(), mkff_audio::Error>(())
/// ```
pub struct AudioWriter {
    enc: Box<dyn Encoder>,
    sink: Sink,
    settings: EncoderSettings,
    frames_in: u64,
}

impl AudioWriter {
    /// Create `path`, choosing the container from its extension. `settings.codec` must fit the
    /// container (e.g. AAC, ALAC, FLAC, MP3 or PCM for `.m4a`); WAV ignores it and writes PCM.
    pub fn create(path: impl AsRef<Path>, settings: EncoderSettings) -> Result<AudioWriter> {
        let path = path.as_ref();
        let container = Container::from_path(path).ok_or_else(|| {
            Error::Unsupported(format!("{}: unknown output container", path.display()))
        })?;
        Self::create_as(path, container, settings)
    }

    /// Like [`Self::create`] with an explicit container.
    pub fn create_as(
        path: impl AsRef<Path>,
        container: Container,
        mut settings: EncoderSettings,
    ) -> Result<AudioWriter> {
        if container == Container::Wav {
            settings.codec = CodecId::Pcm;
        }
        let ok = match container {
            Container::Wav => settings.codec == CodecId::Pcm,
            Container::Mpa => settings.codec == CodecId::MpegAudio,
            Container::Adts => settings.codec == CodecId::Aac,
            Container::Flac => settings.codec == CodecId::Flac,
            Container::M4a => matches!(
                settings.codec,
                CodecId::Aac | CodecId::Alac | CodecId::Flac | CodecId::MpegAudio | CodecId::Pcm
            ),
        };
        if !ok {
            return Err(Error::Unsupported(format!(
                "{} in {container:?}",
                settings.codec.name()
            )));
        }
        let enc = codecs::encoder(&settings)?;
        let file = BufWriter::new(File::create(path.as_ref())?);
        let sink = match container {
            Container::Wav => {
                let fmt = match settings.bits_per_sample {
                    24 => WavSampleFormat::Pcm24,
                    32 => WavSampleFormat::Float32,
                    _ => WavSampleFormat::Pcm16,
                };
                Sink::Wav(WavWriter::new(
                    file,
                    settings.sample_rate,
                    settings.channels as u32,
                    fmt,
                )?)
            }
            Container::Mpa => Sink::Mpa(MpaWriter::new(file, true)?),
            Container::Adts => Sink::Adts(AdtsWriter::new(file, &enc.config())?),
            Container::Flac => Sink::Flac(FlacWriter::new(
                file,
                &enc.config(),
                FlacWriterOptions {
                    seek_points: 100,
                    padding: 4096,
                },
            )?),
            Container::M4a => {
                let mut w = M4aWriter::new(file)?;
                let mut t = M4aTrack::new(
                    settings.codec,
                    settings.sample_rate,
                    settings.channels as u32,
                    enc.config(),
                );
                t.priming = u64::from(enc.priming());
                if settings.codec == CodecId::Pcm {
                    t.pcm = Some(match settings.bits_per_sample {
                        8 => mkff_audio_format::PcmFormat::u8(),
                        24 => mkff_audio_format::PcmFormat::int(24, false),
                        32 => mkff_audio_format::PcmFormat::float(32, false),
                        _ => mkff_audio_format::PcmFormat::int(16, false),
                    });
                }
                let id = w.add_track(t)?;
                Sink::M4a(w, id)
            }
        };
        Ok(AudioWriter {
            enc,
            sink,
            settings,
            frames_in: 0,
        })
    }

    /// Encode planar samples (one slice per channel).
    pub fn write(&mut self, planar: &[&[f32]]) -> Result<()> {
        self.frames_in += planar.first().map_or(0, |c| c.len()) as u64;
        if let Sink::Wav(w) = &mut self.sink {
            let buf = AudioBuffer {
                sample_rate: self.settings.sample_rate,
                channels: planar.iter().map(|c| c.to_vec()).collect(),
            };
            return w.write(&buf);
        }
        let ps = self.enc.encode(planar)?;
        self.put(ps)
    }

    /// Encode a buffer.
    pub fn write_buffer(&mut self, buf: &AudioBuffer) -> Result<()> {
        let refs: Vec<&[f32]> = buf.channels.iter().map(Vec::as_slice).collect();
        self.write(&refs)
    }

    fn put(&mut self, ps: Vec<Packet>) -> Result<()> {
        for p in ps {
            match &mut self.sink {
                Sink::Wav(_) => {}
                Sink::Mpa(w) => w.write_frame(&p.data)?,
                Sink::Adts(w) => w.write_packet(&p.data)?,
                Sink::Flac(w) => w.write_frame(&p.data)?,
                Sink::M4a(w, id) => w.write_packet(*id, &p.data, p.duration)?,
            }
        }
        Ok(())
    }

    /// Samples written so far.
    pub fn frames_written(&self) -> u64 {
        self.frames_in
    }

    /// Flush the encoder and finish the file (headers, tags, index).
    pub fn finish(mut self) -> Result<()> {
        if !matches!(self.sink, Sink::Wav(_)) {
            let ps = self.enc.flush()?;
            self.put(ps)?;
        }
        let total = self.frames_in;
        let priming = u64::from(self.enc.priming());
        let config = self.enc.config();
        let flush = |w: Out| {
            w.into_inner()
                .map_err(|e| Error::Io(e.to_string()))
                .map(drop)
        };
        match self.sink {
            Sink::Wav(w) => flush(w.finish()?),
            Sink::Mpa(w) => {
                // LAME convention: the delay field excludes the 529-sample decoder delay, the
                // padding field includes it
                let frames = w.frames();
                let fs = if self.settings.sample_rate >= 32_000 {
                    1152
                } else {
                    576
                };
                let decoded = frames * fs;
                let delay = priming.saturating_sub(u64::from(mkff_mp3::DECODER_DELAY));
                let padding = decoded.saturating_sub(delay + total);
                let tag = MpaInfoTag::Auto(Some(MpaGapless {
                    encoder_delay: delay.min(4095) as u32,
                    padding: padding.min(4095) as u32,
                }));
                flush(w.finish(tag)?)
            }
            Sink::Adts(w) => flush(w.finish()?),
            Sink::Flac(w) => flush(w.finish(Some(&config))?),
            Sink::M4a(mut w, id) => {
                // gapless: the edit list hides the priming and the end padding
                w.set_duration(id, Some(total))?;
                flush(w.finish()?)
            }
        }
    }
}
