//! Source-backed spectral analysis primitives for the native audio editor.
//!
//! This module deliberately knows nothing about GPUI or project state. PCM is
//! supplied by a host-side worker, and the functions below turn that PCM into
//! bounded, cacheable STFT tiles. Keeping the analysis here makes it testable
//! without starting the application and keeps decode/FFT work off the UI and
//! realtime threads.

use std::sync::Arc;

use rustfft::{FftPlanner, num_complex::Complex32, num_traits::Zero};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEditorViewMode {
    Waveform,
    Spectrogram,
    WaveformOverlay,
    Spectrum,
    Samples,
}

impl AudioEditorViewMode {
    pub const ALL: [Self; 5] = [
        Self::Waveform,
        Self::Spectrogram,
        Self::WaveformOverlay,
        Self::Spectrum,
        Self::Samples,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Waveform => "Waveform",
            Self::Spectrogram => "Spectrogram",
            Self::WaveformOverlay => "Waveform Overlay",
            Self::Spectrum => "Spectrum",
            Self::Samples => "Samples",
        }
    }

    pub const fn shows_waveform(self) -> bool {
        matches!(self, Self::Waveform | Self::WaveformOverlay | Self::Samples)
    }

    pub const fn shows_spectrogram(self) -> bool {
        matches!(self, Self::Spectrogram | Self::WaveformOverlay)
    }

    pub const fn prefers_sample_view(self) -> bool {
        matches!(self, Self::Samples)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmplitudeScale {
    Linear,
    Decibels,
}

impl AmplitudeScale {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Decibels => "dB",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrequencyScale {
    Linear,
    Logarithmic,
}

impl FrequencyScale {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear Hz",
            Self::Logarithmic => "Log Hz",
        }
    }
}

/// Vertical position of `hz` on a spectrogram surface, `0.0` at the bottom of
/// the plotted band and `1.0` at Nyquist.
///
/// Tiles, the frequency ruler, spectral selections, and the repair overlays all
/// route through this function so a band drawn at 60 Hz sits on the 60 Hz row of
/// the image behind it.
pub fn frequency_position(hz: f32, max_frequency_hz: f32, scale: FrequencyScale) -> f32 {
    let max_hz = max_frequency_hz.max(1.0);
    let hz = hz.clamp(0.0, max_hz);
    match scale {
        FrequencyScale::Linear => hz / max_hz,
        FrequencyScale::Logarithmic => {
            let min_hz = spectrogram_min_hz(max_hz);
            ((hz.max(min_hz).ln() - min_hz.ln()) / (max_hz.ln() - min_hz.ln()).max(1.0e-6))
                .clamp(0.0, 1.0)
        }
    }
}

/// Inverse of [`frequency_position`], used by canvas hit-testing.
pub fn position_frequency(position: f32, max_frequency_hz: f32, scale: FrequencyScale) -> f32 {
    let max_hz = max_frequency_hz.max(1.0);
    let position = position.clamp(0.0, 1.0);
    match scale {
        FrequencyScale::Linear => max_hz * position,
        FrequencyScale::Logarithmic => {
            let min_hz = spectrogram_min_hz(max_hz);
            (min_hz.ln() + position * (max_hz.ln() - min_hz.ln())).exp()
        }
    }
}

/// Lowest frequency the logarithmic scale plots. Kept in one place so the tile
/// renderer and the overlays cannot disagree at the bottom of the axis.
///
/// Always 20 Hz. It was written `20.min(max_hz.max(20))`, which is 20 for any
/// rate; a Nyquist below 20 Hz only meets the guard on the divisor in
/// [`frequency_position`].
fn spectrogram_min_hz(_max_frequency_hz: f32) -> f32 {
    20.0
}

#[derive(Debug, Clone, Copy)]
pub struct SpectrogramSettings {
    pub fft_size: usize,
    pub hop_size: usize,
    pub tile_width: usize,
    pub tile_height: usize,
    pub min_db: f32,
    pub max_db: f32,
    pub frequency_scale: FrequencyScale,
}

impl Default for SpectrogramSettings {
    fn default() -> Self {
        Self {
            fft_size: 2048,
            hop_size: 512, // Hann window with 75% overlap.
            tile_width: 256,
            tile_height: 128,
            min_db: -100.0,
            max_db: 0.0,
            frequency_scale: FrequencyScale::Logarithmic,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpectrogramTile {
    pub tile_index: usize,
    pub start_seconds: f32,
    pub duration_seconds: f32,
    pub width: u32,
    pub height: u32,
    /// RGBA pixels, row-major, with high frequencies at the top.
    pub rgba: Arc<[u8]>,
}

#[derive(Debug, Clone)]
pub struct SpectrogramAnalysis {
    pub sample_rate: u32,
    pub total_frames: usize,
    pub duration_seconds: f32,
    pub fft_size: usize,
    pub hop_size: usize,
    pub max_frequency_hz: f32,
    pub tiles: Vec<SpectrogramTile>,
    /// A real, averaged FFT frame used by the Spectrum view.
    pub spectrum_db: Arc<[f32]>,
}

/// A UI-ready tile. The host creates the image off the render path after the
/// worker has completed the FFT, so repainting the panel only reuses texture
/// handles instead of rebuilding pixel buffers.
#[derive(Debug, Clone)]
pub struct SpectrogramTileView {
    pub start_seconds: f32,
    pub duration_seconds: f32,
    pub image: Arc<gpui::RenderImage>,
}

#[derive(Debug, Clone, Default)]
pub struct SpectrogramViewModel {
    pub ready: bool,
    pub status_label: String,
    pub is_error: bool,
    pub tiles: Vec<SpectrogramTileView>,
    pub spectrum_db: Arc<[f32]>,
    pub sample_rate: u32,
    pub max_frequency_hz: f32,
    pub duration_seconds: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpectrogramAnalysisError {
    InvalidSampleRate,
    InvalidSettings,
    EmptyInput,
}

impl std::fmt::Display for SpectrogramAnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSampleRate => f.write_str("invalid sample rate"),
            Self::InvalidSettings => f.write_str("invalid spectrogram settings"),
            Self::EmptyInput => f.write_str("audio source contains no samples"),
        }
    }
}

impl std::error::Error for SpectrogramAnalysisError {}

/// Compute a source-backed tiled STFT and an averaged spectrum.
pub fn analyze_spectrogram(
    mono_samples: &[f32],
    sample_rate: u32,
    settings: SpectrogramSettings,
) -> Result<SpectrogramAnalysis, SpectrogramAnalysisError> {
    if sample_rate == 0 {
        return Err(SpectrogramAnalysisError::InvalidSampleRate);
    }
    if mono_samples.is_empty() {
        return Err(SpectrogramAnalysisError::EmptyInput);
    }
    if settings.fft_size < 2
        || !settings.fft_size.is_power_of_two()
        || settings.hop_size == 0
        || settings.hop_size > settings.fft_size
        || settings.tile_width == 0
        || settings.tile_height < 2
        || settings.min_db >= settings.max_db
    {
        return Err(SpectrogramAnalysisError::InvalidSettings);
    }

    let fft_size = settings.fft_size;
    let bin_count = fft_size / 2 + 1;
    let frame_count = ((mono_samples.len().saturating_sub(1)) / settings.hop_size) + 1;
    let columns_per_tile = settings.tile_width;
    let tile_count = frame_count.div_ceil(columns_per_tile);
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_size);
    let window: Vec<f32> = (0..fft_size)
        .map(|i| {
            let phase = std::f32::consts::TAU * i as f32 / (fft_size - 1) as f32;
            0.5 - 0.5 * phase.cos()
        })
        .collect();
    let max_frequency_hz = sample_rate as f32 * 0.5;
    let db_span = settings.max_db - settings.min_db;

    let mut tiles = Vec::with_capacity(tile_count);
    let mut average_power = vec![0.0_f32; bin_count];
    let mut scratch = vec![Complex32::zero(); fft_size];
    let mut tile_values = vec![0.0_f32; columns_per_tile * settings.tile_height];

    for frame_index in 0..frame_count {
        scratch.fill(Complex32::zero());
        let sample_start = frame_index * settings.hop_size;
        for (i, value) in scratch.iter_mut().enumerate() {
            let sample = mono_samples.get(sample_start + i).copied().unwrap_or(0.0);
            value.re = sample * window[i];
        }
        fft.process(&mut scratch);

        let mut db_bins = vec![settings.min_db; bin_count];
        for bin in 0..bin_count {
            let power = scratch[bin].norm_sqr() / (fft_size as f32 * fft_size as f32);
            average_power[bin] += power;
            db_bins[bin] =
                (10.0 * power.max(1e-20).log10()).clamp(settings.min_db, settings.max_db);
        }

        let tile_index = frame_index / columns_per_tile;
        let x = frame_index % columns_per_tile;
        for y in 0..settings.tile_height {
            let bin = frequency_bin_for_row(
                y,
                settings.tile_height,
                bin_count,
                max_frequency_hz,
                settings.frequency_scale,
            );
            tile_values[y * columns_per_tile + x] = db_bins[bin];
        }

        let is_last_column = x + 1 == columns_per_tile || frame_index + 1 == frame_count;
        if is_last_column {
            let width = if x + 1 == columns_per_tile {
                columns_per_tile
            } else {
                x + 1
            };
            let rgba = tile_to_rgba(
                &tile_values,
                columns_per_tile,
                width,
                settings.tile_height,
                settings.min_db,
                db_span,
            );
            let start_frame = tile_index * columns_per_tile * settings.hop_size;
            let end_frame = (start_frame + width * settings.hop_size).min(mono_samples.len());
            tiles.push(SpectrogramTile {
                tile_index,
                start_seconds: start_frame as f32 / sample_rate as f32,
                duration_seconds: (end_frame.saturating_sub(start_frame)) as f32
                    / sample_rate as f32,
                width: width as u32,
                height: settings.tile_height as u32,
                rgba,
            });
            tile_values.fill(settings.min_db);
        }
    }

    let spectrum_db: Arc<[f32]> = average_power
        .into_iter()
        .map(|power| {
            (10.0 * (power / frame_count as f32).max(1e-20).log10())
                .clamp(settings.min_db, settings.max_db)
        })
        .collect::<Vec<_>>()
        .into();

    Ok(SpectrogramAnalysis {
        sample_rate,
        total_frames: mono_samples.len(),
        duration_seconds: mono_samples.len() as f32 / sample_rate as f32,
        fft_size,
        hop_size: settings.hop_size,
        max_frequency_hz,
        tiles,
        spectrum_db,
    })
}

fn frequency_bin_for_row(
    row: usize,
    height: usize,
    bin_count: usize,
    max_frequency_hz: f32,
    scale: FrequencyScale,
) -> usize {
    let t = row as f32 / (height - 1) as f32;
    let frequency = position_frequency(1.0 - t, max_frequency_hz, scale);
    ((frequency / max_frequency_hz.max(1.0)) * (bin_count - 1) as f32)
        .round()
        .clamp(0.0, (bin_count - 1) as f32) as usize
}

fn tile_to_rgba(
    values: &[f32],
    stride: usize,
    width: usize,
    height: usize,
    min_db: f32,
    db_span: f32,
) -> Arc<[u8]> {
    let mut pixels = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let db = values[y * stride.max(1) + x];
            let value = ((db - min_db) / db_span).clamp(0.0, 1.0);
            let (r, g, b) = spectral_color(value);
            pixels.extend_from_slice(&[r, g, b, 255]);
        }
    }
    pixels.into()
}

/// A theme-derived dark blue/purple/amber ramp. It intentionally avoids the
/// misleading high-contrast jet/rainbow palette used by scientific plots.
fn spectral_color(value: f32) -> (u8, u8, u8) {
    let stops = [
        (0.00, (5.0, 8.0, 16.0)),
        (0.28, (16.0, 36.0, 76.0)),
        (0.56, (48.0, 92.0, 140.0)),
        (0.78, (126.0, 82.0, 164.0)),
        (1.00, (239.0, 178.0, 86.0)),
    ];
    let value = value.clamp(0.0, 1.0);
    for pair in stops.windows(2) {
        let (a, ca) = pair[0];
        let (b, cb) = pair[1];
        if value <= b {
            let t = ((value - a) / (b - a)).clamp(0.0, 1.0);
            return (
                (ca.0 + (cb.0 - ca.0) * t) as u8,
                (ca.1 + (cb.1 - ca.1) * t) as u8,
                (ca.2 + (cb.2 - ca.2) * t) as u8,
            );
        }
    }
    (239, 178, 86)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_energy_lands_near_the_expected_frequency() {
        let sample_rate = 48_000;
        let frequency = 1_000.0;
        let samples: Vec<f32> = (0..384_000)
            .map(|i| (std::f32::consts::TAU * frequency * i as f32 / sample_rate as f32).sin())
            .collect();
        let analysis = analyze_spectrogram(&samples, sample_rate, SpectrogramSettings::default())
            .expect("analysis should succeed");
        let peak_bin = analysis
            .spectrum_db
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(bin, _)| bin)
            .unwrap();
        let peak_hz =
            peak_bin as f32 * sample_rate as f32 / SpectrogramSettings::default().fft_size as f32;
        assert!((peak_hz - frequency).abs() < 80.0, "peak={peak_hz}Hz");
        assert!(analysis.tiles.len() > 1);
        assert!(analysis.tiles.iter().all(|tile| !tile.rgba.is_empty()));
    }

    #[test]
    fn invalid_input_is_rejected_without_allocating_tiles() {
        assert!(matches!(
            analyze_spectrogram(&[0.0], 0, SpectrogramSettings::default()),
            Err(SpectrogramAnalysisError::InvalidSampleRate)
        ));
        assert!(matches!(
            analyze_spectrogram(&[], 48_000, SpectrogramSettings::default()),
            Err(SpectrogramAnalysisError::EmptyInput)
        ));
    }
}
