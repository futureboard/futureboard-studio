//! MKFF's audio codecs as a Media Kit add-on: a software decoder flavor for every codec and
//! encoder flavors for MP3, AAC, FLAC and ALAC.

use std::sync::{Arc, OnceLock};

use mkff_audio_core::{CodecId, Decoder, Encoder};
use mkff_mediakit::{
    Acceleration, AudioChunk, AudioDecoderPlugin, AudioEncoderPlugin, CodecFamily, EncodedChunk,
    FlavorInfo, Instance, Kinds, MediaAddOn, MediaFormat, MediaRoster, RawAudioFormat,
    SampleFormat,
};

use crate::codecs::{self, EncoderSettings};

/// Media Kit family of a codec.
pub fn family(c: CodecId) -> CodecFamily {
    match c {
        CodecId::Pcm => CodecFamily::Pcm,
        CodecId::MpegAudio => CodecFamily::MpegAudio,
        CodecId::Aac => CodecFamily::Aac,
        CodecId::Flac => CodecFamily::Flac,
        CodecId::Alac => CodecFamily::Alac,
        CodecId::Ac3 => CodecFamily::Ac3,
        CodecId::Opus => CodecFamily::Opus,
        CodecId::Vorbis => CodecFamily::Vorbis,
    }
}

/// Codec of a Media Kit audio family.
pub fn codec_id(f: CodecFamily) -> Option<CodecId> {
    Some(match f {
        CodecFamily::Pcm => CodecId::Pcm,
        CodecFamily::MpegAudio => CodecId::MpegAudio,
        CodecFamily::Aac => CodecId::Aac,
        CodecFamily::Flac => CodecId::Flac,
        CodecFamily::Alac => CodecId::Alac,
        CodecFamily::Ac3 => CodecId::Ac3,
        CodecFamily::Opus => CodecId::Opus,
        CodecFamily::Vorbis => CodecId::Vorbis,
        _ => return None,
    })
}

/// Flavor ids: decoders 0..; encoders 100..
const ENCODER_BASE: u32 = 100;

/// The add-on.
pub struct SoftwareAudioAddOn;

fn mk(e: mkff_audio_core::Error) -> mkff_mediakit::Error {
    match e {
        mkff_audio_core::Error::Unsupported(s) => mkff_mediakit::Error::Unsupported(s),
        mkff_audio_core::Error::InvalidArgument(s) => mkff_mediakit::Error::InvalidArgument(s),
        other => mkff_mediakit::Error::Codec(other.to_string()),
    }
}

impl MediaAddOn for SoftwareAudioAddOn {
    fn name(&self) -> &str {
        "MKFF audio codecs"
    }

    fn flavors(&self) -> Vec<FlavorInfo> {
        let mut v = Vec::new();
        for (i, c) in codecs::DECODABLE.iter().enumerate() {
            if *c == CodecId::Pcm {
                continue;
            }
            v.push(FlavorInfo {
                id: i as u32,
                name: format!("{} decoder", c.name()),
                info: "MKFF software decoder (pure Rust)".into(),
                kinds: Kinds::DECODER,
                accel: Acceleration::Software,
                in_formats: vec![MediaFormat::encoded(family(*c), Vec::new(), 0, 0)],
                out_formats: vec![MediaFormat::raw_audio(0, 0, SampleFormat::F32Planar)],
                rank: 100,
            });
        }
        for (i, c) in codecs::ENCODABLE.iter().enumerate() {
            if *c == CodecId::Pcm {
                continue;
            }
            v.push(FlavorInfo {
                id: ENCODER_BASE + i as u32,
                name: format!("{} encoder", c.name()),
                info: "MKFF software encoder (pure Rust)".into(),
                kinds: Kinds::ENCODER,
                accel: Acceleration::Software,
                in_formats: vec![MediaFormat::raw_audio(0, 0, SampleFormat::Any)],
                out_formats: vec![MediaFormat::encoded(family(*c), Vec::new(), 0, 0)],
                rank: 100,
            });
        }
        v
    }

    fn instantiate(
        &self,
        flavor_id: u32,
        input: &MediaFormat,
        output: &MediaFormat,
    ) -> mkff_mediakit::Result<Instance> {
        if flavor_id < ENCODER_BASE {
            let c = *codecs::DECODABLE
                .get(flavor_id as usize)
                .ok_or_else(|| mkff_mediakit::Error::BadNode(format!("flavor {flavor_id}")))?;
            let MediaFormat::EncodedAudio(e) = input else {
                return Err(mkff_mediakit::Error::BadFormat(format!(
                    "{} decoder given {input}",
                    c.name()
                )));
            };
            let dec = codecs::decoder(c, &e.config).map_err(mk)?;
            return Ok(Instance::AudioDecoder(Box::new(DecoderPlugin {
                dec,
                hint: (e.a, e.b),
            })));
        }
        let c = *codecs::ENCODABLE
            .get((flavor_id - ENCODER_BASE) as usize)
            .ok_or_else(|| mkff_mediakit::Error::BadNode(format!("flavor {flavor_id}")))?;
        let MediaFormat::RawAudio(a) = input else {
            return Err(mkff_mediakit::Error::BadFormat(format!(
                "{} encoder given {input}",
                c.name()
            )));
        };
        if a.frame_rate == 0 || a.channels == 0 {
            return Err(mkff_mediakit::Error::BadFormat(format!(
                "{} encoder needs a rate and channel count",
                c.name()
            )));
        }
        let mut s = EncoderSettings::new(c, a.frame_rate, a.channels as usize);
        if let MediaFormat::EncodedAudio(o) = output {
            s.bitrate = o.bit_rate;
        }
        let enc = codecs::encoder(&s).map_err(mk)?;
        Ok(Instance::AudioEncoder(Box::new(EncoderPlugin {
            enc,
            family: family(c),
            bitrate: s.bitrate,
        })))
    }
}

struct DecoderPlugin {
    dec: Box<dyn Decoder>,
    /// Rate / channels the container reported.
    hint: (u32, u32),
}

impl AudioDecoderPlugin for DecoderPlugin {
    fn decode(&mut self, packet: &[u8], pts: i64) -> mkff_mediakit::Result<AudioChunk> {
        let b = self.dec.decode(packet).map_err(mk)?;
        Ok(AudioChunk {
            sample_rate: b.sample_rate,
            channels: b.channels,
            pts,
        })
    }

    fn reset(&mut self) {
        self.dec.reset();
    }

    fn output_format(&self) -> RawAudioFormat {
        let rate = match self.dec.sample_rate() {
            0 => self.hint.0,
            r => r,
        };
        let ch = match self.dec.channels() {
            0 => self.hint.1,
            c => c as u32,
        };
        RawAudioFormat {
            frame_rate: rate,
            channels: ch,
            format: SampleFormat::F32Planar,
            buffer_frames: 0,
        }
    }
}

struct EncoderPlugin {
    enc: Box<dyn Encoder>,
    family: CodecFamily,
    bitrate: u32,
}

impl EncoderPlugin {
    fn chunks(&self, ps: Vec<mkff_audio_core::Packet>) -> Vec<EncodedChunk> {
        let rate = i64::from(self.enc.sample_rate().max(1));
        let us = |s: i64| s.saturating_mul(1_000_000) / rate;
        ps.into_iter()
            .map(|p| EncodedChunk {
                pts: us(p.pts),
                dts: us(p.pts),
                duration: us(i64::from(p.duration)),
                keyframe: true,
                data: p.data,
            })
            .collect()
    }
}

impl AudioEncoderPlugin for EncoderPlugin {
    fn encode(&mut self, planar: &[&[f32]]) -> mkff_mediakit::Result<Vec<EncodedChunk>> {
        let ps = self.enc.encode(planar).map_err(mk)?;
        Ok(self.chunks(ps))
    }

    fn flush(&mut self) -> mkff_mediakit::Result<Vec<EncodedChunk>> {
        let ps = self.enc.flush().map_err(mk)?;
        Ok(self.chunks(ps))
    }

    fn output_format(&self) -> MediaFormat {
        let mut f = MediaFormat::encoded(
            self.family,
            self.enc.config(),
            self.enc.sample_rate(),
            self.enc.channels() as u32,
        );
        if let MediaFormat::EncodedAudio(e) = &mut f {
            e.bit_rate = self.bitrate;
        }
        f
    }
}

/// Register the add-on with `roster`.
pub fn register(roster: &MediaRoster) {
    roster.register_addon(Arc::new(SoftwareAudioAddOn));
}

/// The process-wide roster with MKFF's audio codecs registered (once).
pub fn roster() -> &'static MediaRoster {
    static DONE: OnceLock<()> = OnceLock::new();
    let r = MediaRoster::roster();
    DONE.get_or_init(|| register(r));
    r
}
