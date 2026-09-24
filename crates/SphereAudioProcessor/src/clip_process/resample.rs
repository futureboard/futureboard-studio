//! Offline sample-rate conversion via rubato. Worker thread only.

use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    #[error("invalid sample rate")]
    InvalidRate,
    #[error("resampler: {0}")]
    Rubato(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Resample interleaved PCM from `source_rate` to `target_rate`.
pub fn resample_interleaved(
    samples: &[f32],
    channels: usize,
    source_rate: u32,
    target_rate: u32,
) -> Result<Vec<f32>, ResampleError> {
    let channels = channels.max(1);
    if source_rate == 0 || target_rate == 0 {
        return Err(ResampleError::InvalidRate);
    }
    if source_rate == target_rate {
        return Ok(samples.to_vec());
    }
    let frames = samples.len() / channels;
    if frames == 0 {
        return Ok(Vec::new());
    }
    let mut planar_in = vec![vec![0.0_f32; frames]; channels];
    for (frame, chunk) in samples.chunks(channels).enumerate() {
        for ch in 0..channels {
            planar_in[ch][frame] = chunk.get(ch).copied().unwrap_or(0.0);
        }
    }
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    let chunk = 1024.min(frames).max(64);
    let mut resampler = SincFixedIn::<f32>::new(
        target_rate as f64 / source_rate as f64,
        2.0,
        params,
        chunk,
        channels,
    )
    .map_err(|e| ResampleError::Rubato(e.to_string()))?;

    let mut planar_out = vec![Vec::new(); channels];
    let mut offset = 0usize;
    while offset < frames {
        let end = (offset + chunk).min(frames);
        let mut block: Vec<Vec<f32>> = (0..channels)
            .map(|ch| {
                let mut row = planar_in[ch][offset..end].to_vec();
                if row.len() < chunk {
                    row.resize(chunk, 0.0);
                }
                row
            })
            .collect();
        if end == frames && end - offset < chunk {
            let waves = resampler
                .process_partial(Some(&block), None)
                .map_err(|e| ResampleError::Rubato(e.to_string()))?;
            for ch in 0..channels {
                planar_out[ch].extend_from_slice(&waves[ch]);
            }
            break;
        }
        let waves = resampler
            .process(&block, None)
            .map_err(|e| ResampleError::Rubato(e.to_string()))?;
        for ch in 0..channels {
            planar_out[ch].extend_from_slice(&waves[ch]);
        }
        let _ = &mut block;
        offset = end;
    }

    let out_frames = planar_out.first().map(|c| c.len()).unwrap_or(0);
    let mut interleaved = Vec::with_capacity(out_frames * channels);
    for frame in 0..out_frames {
        for ch in 0..channels {
            interleaved.push(planar_out[ch][frame]);
        }
    }
    Ok(interleaved)
}

/// Write interleaved PCM as a 32-bit IEEE-float WAV. Used for every derived
/// (edited/processed) asset.
///
/// Float keeps the full resolution of 24-bit and float sources and preserves
/// peaks above 0 dBFS instead of clipping them — an edit must never cost the
/// audio quality. The file is written to a sibling temp path and renamed into
/// place, so a failed or interrupted write never leaves a truncated file where
/// a clip expects audio.
pub fn write_wav_f32(
    path: &Path,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
) -> Result<(), ResampleError> {
    let channels = channels.max(1);
    let frames = samples.len() / channels as usize;
    let block_align = channels as u32 * 4;
    let data_bytes = frames as u32 * block_align;
    // fmt (18 bytes, cbSize = 0) + fact chunk, as the spec asks for
    // non-PCM formats.
    let mut bytes = Vec::with_capacity(58 + data_bytes as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(50 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&18u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * block_align).to_le_bytes());
    bytes.extend_from_slice(&(block_align as u16).to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes()); // cbSize
    bytes.extend_from_slice(b"fact");
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&(frames as u32).to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples.iter().take(frames * channels as usize) {
        let value = if sample.is_finite() { *sample } else { 0.0 };
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let tmp = path.with_extension("wav.tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_wav_keeps_resolution_and_overs() {
        let dir = std::env::temp_dir().join(format!("fb-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("float.wav");
        let samples = [0.123_456_78_f32, -0.5, 1.5, -1.25, f32::NAN, 0.0];
        write_wav_f32(&path, &samples, 2, 48_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[20..22], &3u16.to_le_bytes(), "IEEE float format");
        assert_eq!(&bytes[34..36], &32u16.to_le_bytes(), "32-bit");
        let data = bytes.windows(4).position(|w| w == b"data").unwrap() + 8;
        let read: Vec<f32> = bytes[data..]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert_eq!(read, vec![0.123_456_78, -0.5, 1.5, -1.25, 0.0, 0.0]);
        assert!(!path.with_extension("wav.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_rate_copies() {
        let samples = vec![0.1, -0.2, 0.3, -0.4];
        let out = resample_interleaved(&samples, 2, 48_000, 48_000).unwrap();
        assert_eq!(out, samples);
    }
}
