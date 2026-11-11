//! A minimal RIFF/WAVE reader for impulse-response files.
//!
//! Deliberately narrow: it decodes the formats cabinet IRs actually ship in
//! (PCM 8/16/24/32-bit and IEEE float 32/64, mono through 8 channels) and
//! nothing else — no compressed codecs, no streaming, no seeking. That keeps
//! the crate free of an audio-decoding dependency for a job that is a few
//! hundred lines of header parsing.
//!
//! Control thread only: parsing allocates the decoded sample buffer.

/// A `.wav` payload that could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WavError {
    /// Not a RIFF/WAVE container at all.
    NotWave,
    /// A chunk header or body runs past the end of the file.
    Truncated,
    /// Well-formed RIFF, but a format this reader does not decode.
    Unsupported(&'static str),
    /// No `data` chunk, or a `data` chunk holding zero frames.
    NoAudio,
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WavError::NotWave => write!(f, "not a RIFF/WAVE file"),
            WavError::Truncated => write!(f, "WAV file is truncated"),
            WavError::Unsupported(what) => write!(f, "unsupported WAV format: {what}"),
            WavError::NoAudio => write!(f, "WAV file contains no audio frames"),
        }
    }
}

impl std::error::Error for WavError {}

/// Decoded interleaved audio, normalized to `f32` in roughly -1..1.
#[derive(Debug, Clone)]
pub struct WavAudio {
    /// Interleaved samples, `channels` per frame.
    pub samples: Vec<f32>,
    pub channels: usize,
    pub sample_rate: f64,
}

impl WavAudio {
    pub fn frames(&self) -> usize {
        self.samples.len().checked_div(self.channels).unwrap_or(0)
    }

    /// One channel's samples, deinterleaved into `out` (resized to fit).
    /// Channels past the end of the file repeat the last one, so a mono file
    /// answers for both sides of a stereo request.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn channel_into(&self, channel: usize, out: &mut Vec<f32>) {
        self.channel_prefix_into(channel, usize::MAX, out);
    }

    /// Like [`Self::channel_into`], but only the first `max_frames` frames:
    /// a caller that keeps a short prefix never holds the whole file twice.
    pub fn channel_prefix_into(&self, channel: usize, max_frames: usize, out: &mut Vec<f32>) {
        let frames = self.frames().min(max_frames);
        out.clear();
        out.reserve(frames);
        if self.channels == 0 {
            return;
        }
        let ch = channel.min(self.channels - 1);
        for frame in 0..frames {
            out.push(self.samples[frame * self.channels + ch]);
        }
    }
}

// WAVE format tags.
const FORMAT_PCM: u16 = 1;
const FORMAT_IEEE_FLOAT: u16 = 3;
const FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Widest file this reader will decode. An IR is milliseconds long; anything
/// approaching this is a whole song handed to the wrong loader.
const MAX_BYTES: usize = 64 * 1024 * 1024;
/// Most channels a WAVE file may declare here (7.1 and below).
const MAX_CHANNELS: usize = 8;

fn u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Parse a whole `.wav` file held in memory.
pub fn parse_wav(bytes: &[u8]) -> Result<WavAudio, WavError> {
    if bytes.len() > MAX_BYTES {
        return Err(WavError::Unsupported("file is too large for an IR"));
    }
    if bytes.len() < 12 {
        return Err(WavError::Truncated);
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotWave);
    }

    let mut format_tag: Option<u16> = None;
    let mut channels = 0usize;
    let mut sample_rate = 0f64;
    let mut bits_per_sample = 0u16;
    let mut data: Option<&[u8]> = None;

    // Walk the chunk list. Unknown chunks (LIST, fact, cue, smpl…) are skipped
    // by their declared size; chunk bodies are word-aligned.
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32_le(bytes, pos + 4).ok_or(WavError::Truncated)? as usize;
        let body_start = pos + 8;
        let body_end = body_start.checked_add(size).ok_or(WavError::Truncated)?;
        if body_end > bytes.len() {
            // A `data` chunk whose declared size overruns the file is common
            // in truncated downloads — take what is actually there.
            if id == b"data" {
                data = Some(&bytes[body_start..]);
                break;
            }
            return Err(WavError::Truncated);
        }
        let body = &bytes[body_start..body_end];

        if id == b"fmt " {
            if body.len() < 16 {
                return Err(WavError::Truncated);
            }
            let mut tag = u16_le(body, 0).ok_or(WavError::Truncated)?;
            channels = u16_le(body, 2).ok_or(WavError::Truncated)? as usize;
            sample_rate = u32_le(body, 4).ok_or(WavError::Truncated)? as f64;
            bits_per_sample = u16_le(body, 14).ok_or(WavError::Truncated)?;
            if tag == FORMAT_EXTENSIBLE {
                // The real format is the first two bytes of the SubFormat GUID
                // in the extension block.
                if body.len() < 40 {
                    return Err(WavError::Unsupported("truncated WAVE_FORMAT_EXTENSIBLE"));
                }
                tag = u16_le(body, 24).ok_or(WavError::Truncated)?;
            }
            format_tag = Some(tag);
        } else if id == b"data" {
            data = Some(body);
        }

        pos = body_end + (body_end & 1); // chunks are word-aligned
    }

    let format_tag = format_tag.ok_or(WavError::Unsupported("missing fmt chunk"))?;
    let data = data.ok_or(WavError::NoAudio)?;
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(WavError::Unsupported("unusable channel count"));
    }
    if !(sample_rate.is_finite() && sample_rate >= 1.0) {
        return Err(WavError::Unsupported("unusable sample rate"));
    }

    let samples = decode_samples(data, format_tag, bits_per_sample)?;
    if samples.len() < channels {
        return Err(WavError::NoAudio);
    }
    // Drop a trailing partial frame rather than misaligning every channel.
    let frames = samples.len() / channels;
    let mut samples = samples;
    samples.truncate(frames * channels);

    Ok(WavAudio {
        samples,
        channels,
        sample_rate,
    })
}

fn decode_samples(data: &[u8], format_tag: u16, bits: u16) -> Result<Vec<f32>, WavError> {
    match (format_tag, bits) {
        // 8-bit PCM is unsigned with a 128 offset; everything wider is signed.
        (FORMAT_PCM, 8) => Ok(data.iter().map(|&b| (b as f32 - 128.0) / 128.0).collect()),
        (FORMAT_PCM, 16) => Ok(data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32_768.0)
            .collect()),
        (FORMAT_PCM, 24) => Ok(data
            .chunks_exact(3)
            .map(|c| {
                // Sign-extend the 24-bit little-endian value into an i32.
                let v = i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8;
                v as f32 / 8_388_608.0
            })
            .collect()),
        (FORMAT_PCM, 32) => Ok(data
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
            .collect()),
        (FORMAT_IEEE_FLOAT, 32) => Ok(data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()),
        (FORMAT_IEEE_FLOAT, 64) => Ok(data
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) as f32)
            .collect()),
        (FORMAT_PCM, _) => Err(WavError::Unsupported("PCM bit depth")),
        (FORMAT_IEEE_FLOAT, _) => Err(WavError::Unsupported("float bit depth")),
        _ => Err(WavError::Unsupported("compressed or unknown codec")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a canonical WAVE file around an already-encoded data payload.
    fn wav(format_tag: u16, bits: u16, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let block_align = channels * bits / 8;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&format_tag.to_le_bytes());
        fmt.extend_from_slice(&channels.to_le_bytes());
        fmt.extend_from_slice(&rate.to_le_bytes());
        fmt.extend_from_slice(&(rate * block_align as u32).to_le_bytes());
        fmt.extend_from_slice(&block_align.to_le_bytes());
        fmt.extend_from_slice(&bits.to_le_bytes());

        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn decodes_16_bit_pcm_mono() {
        let data: Vec<u8> = [0i16, 16_384, -16_384, 32_767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let audio = parse_wav(&wav(FORMAT_PCM, 16, 1, 48_000, &data)).expect("decodes");
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.sample_rate, 48_000.0);
        assert_eq!(audio.frames(), 4);
        assert!((audio.samples[1] - 0.5).abs() < 1.0e-4);
        assert!((audio.samples[2] + 0.5).abs() < 1.0e-4);
    }

    #[test]
    fn decodes_24_bit_pcm_with_correct_sign() {
        // +0.5 and -0.5 as 24-bit little-endian.
        let data = [0x00, 0x00, 0x40, 0x00, 0x00, 0xC0];
        let audio = parse_wav(&wav(FORMAT_PCM, 24, 1, 44_100, &data)).expect("decodes");
        assert_eq!(audio.frames(), 2);
        assert!(
            (audio.samples[0] - 0.5).abs() < 1.0e-4,
            "{}",
            audio.samples[0]
        );
        assert!(
            (audio.samples[1] + 0.5).abs() < 1.0e-4,
            "{}",
            audio.samples[1]
        );
    }

    #[test]
    fn decodes_32_bit_float_stereo_and_deinterleaves() {
        let data: Vec<u8> = [0.25f32, -0.75, 0.5, -1.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let audio = parse_wav(&wav(FORMAT_IEEE_FLOAT, 32, 2, 96_000, &data)).expect("decodes");
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.frames(), 2);
        let mut left = Vec::new();
        let mut right = Vec::new();
        audio.channel_into(0, &mut left);
        audio.channel_into(1, &mut right);
        assert_eq!(left, vec![0.25, 0.5]);
        assert_eq!(right, vec![-0.75, -1.0]);
    }

    #[test]
    fn a_mono_file_answers_for_both_stereo_sides() {
        let data: Vec<u8> = [0.25f32, 0.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let audio = parse_wav(&wav(FORMAT_IEEE_FLOAT, 32, 1, 48_000, &data)).expect("decodes");
        let mut left = Vec::new();
        let mut right = Vec::new();
        audio.channel_into(0, &mut left);
        audio.channel_into(1, &mut right);
        assert_eq!(left, right);
    }

    #[test]
    fn skips_unknown_chunks_before_the_data_chunk() {
        let mut file = wav(FORMAT_PCM, 16, 1, 48_000, &[0u8, 0, 0, 0]);
        // Splice a LIST chunk in right after the WAVE tag.
        let list: Vec<u8> = b"LIST\x06\x00\x00\x00INFOxx".to_vec();
        file.splice(12..12, list.iter().copied());
        let riff_size = (file.len() - 8) as u32;
        file[4..8].copy_from_slice(&riff_size.to_le_bytes());
        let audio = parse_wav(&file).expect("unknown chunks must be skipped");
        assert_eq!(audio.frames(), 2);
    }

    #[test]
    fn resolves_the_real_format_behind_wave_format_extensible() {
        // A 16-byte fmt chunk declaring EXTENSIBLE has no SubFormat GUID, so
        // build the full 40-byte extension form by hand.
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&FORMAT_EXTENSIBLE.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes()); // channels
        fmt.extend_from_slice(&48_000u32.to_le_bytes());
        fmt.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        fmt.extend_from_slice(&4u16.to_le_bytes()); // block align
        fmt.extend_from_slice(&32u16.to_le_bytes()); // bits
        fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&32u16.to_le_bytes()); // valid bits
        fmt.extend_from_slice(&0u32.to_le_bytes()); // channel mask
        fmt.extend_from_slice(&FORMAT_IEEE_FLOAT.to_le_bytes()); // SubFormat GUID head
        fmt.extend_from_slice(&[0u8; 14]);

        let data: Vec<u8> = [0.5f32, -0.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut file = Vec::new();
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
        file.extend_from_slice(b"WAVE");
        file.extend_from_slice(b"fmt ");
        file.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        file.extend_from_slice(&fmt);
        file.extend_from_slice(b"data");
        file.extend_from_slice(&(data.len() as u32).to_le_bytes());
        file.extend_from_slice(&data);

        let audio = parse_wav(&file).expect("extensible float must decode");
        assert_eq!(audio.frames(), 2);
        assert!((audio.samples[0] - 0.5).abs() < 1.0e-6);
    }

    #[test]
    fn an_overrunning_data_chunk_keeps_what_is_actually_present() {
        let mut file = wav(FORMAT_PCM, 16, 1, 48_000, &[0u8, 0, 0, 0]);
        // Claim far more data than the file holds (truncated download).
        let len = file.len();
        file[len - 4 - 4..len - 4].copy_from_slice(&9_999u32.to_le_bytes());
        let audio = parse_wav(&file).expect("a short read must still decode");
        assert_eq!(audio.frames(), 2);
    }

    #[test]
    fn rejects_non_wave_and_unsupported_payloads() {
        assert_eq!(
            parse_wav(b"not a wav at all!!!!").unwrap_err(),
            WavError::NotWave
        );
        assert_eq!(parse_wav(&[]).unwrap_err(), WavError::Truncated);
        // ADPCM (tag 2) is a real WAVE codec this reader does not decode.
        assert!(matches!(
            parse_wav(&wav(2, 4, 1, 48_000, &[0u8; 8])).unwrap_err(),
            WavError::Unsupported(_)
        ));
        // Zero channels is structurally unusable.
        assert!(matches!(
            parse_wav(&wav(FORMAT_PCM, 16, 0, 48_000, &[0u8; 8])).unwrap_err(),
            WavError::Unsupported(_)
        ));
        // A well-formed header with no frames.
        assert_eq!(
            parse_wav(&wav(FORMAT_PCM, 16, 2, 48_000, &[])).unwrap_err(),
            WavError::NoAudio
        );
    }

    #[test]
    fn never_panics_on_arbitrary_bytes() {
        // Header-shaped garbage is the realistic hostile input: a valid RIFF
        // preamble followed by nonsense chunk sizes.
        for seed in 0..256u32 {
            let mut file: Vec<u8> = b"RIFF\xff\xff\xff\xffWAVE".to_vec();
            for i in 0..64u32 {
                file.push(((seed.wrapping_mul(2_654_435_761).wrapping_add(i)) >> 3) as u8);
            }
            let _ = parse_wav(&file);
        }
    }
}
