//! Real MDX-NET stem separation via ONNX Runtime (`ort`).
//!
//! Classification: scanner/offline path (worker thread only) — never call from
//! the realtime audio callback. It allocates freely, runs FFTs, and blocks on
//! ONNX inference.
//!
//! Pipeline (per UVR MDX-NET): resample to 44.1 kHz → chunked STFT →
//! `[1, 4, dim_f, dim_t]` model input → ONNX inference → masked spectrogram →
//! iSTFT / overlap-add → loudness compensation → resample back to the input
//! rate. Stereo is processed as two channels folded into the 4-channel
//! (real/imag × L/R) MDX input.
//!
//! GPU: the ONNX Runtime shared library is loaded dynamically (`load-dynamic`).
//! When built with `cuda` / `directml` / `coreml` and the device resolves to
//! GPU, those execution providers are tried first, then CPU. Set
//! `ORT_DYLIB_PATH` to point at a matching (optionally GPU-enabled)
//! `onnxruntime` library.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ndarray::Array4;
use ort::execution_providers::ExecutionProviderDispatch;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{Tensor, ValueType};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use super::dsp::{
    PlanarStereo, backend_err, deinterleave_stereo, interleave_stereo, resample_planar,
};
use super::mdx_params::{MdxParams, params_for_file};
use super::{InferBackendKind, SeparatedStem, StemInferBackend};
use crate::device::InferDevice;
use crate::error::StemExtractError;
use crate::model::StemModel;
use crate::params::StemExtractParams;
use crate::progress::{StemExtractCancelToken, StemExtractProgress, StemExtractStage};
use crate::stems::StemKind;

/// Execution providers to try, in priority order, for the resolved device.
fn execution_providers(device: InferDevice) -> Vec<ExecutionProviderDispatch> {
    let mut providers = Vec::new();
    if device == InferDevice::Gpu {
        #[cfg(feature = "cuda")]
        providers.push(ort::execution_providers::CUDAExecutionProvider::default().build());
        #[cfg(feature = "directml")]
        providers.push(ort::execution_providers::DirectMLExecutionProvider::default().build());
        #[cfg(feature = "coreml")]
        providers.push(ort::execution_providers::CoreMLExecutionProvider::default().build());
    }
    // CPU is always the final fallback so a missing GPU EP degrades gracefully.
    providers.push(ort::execution_providers::CPUExecutionProvider::default().build());
    providers
}

/// One loaded MDX-NET ONNX model plus its resolved STFT geometry.
struct MdxModel {
    session: Session,
    params: MdxParams,
    input_name: String,
    output_name: String,
    fwd: Arc<dyn Fft<f32>>,
    inv: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
}

impl MdxModel {
    fn load(path: &Path, device: InferDevice) -> Result<Self, StemExtractError> {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let mut params = params_for_file(file_name);

        let session = Session::builder()
            .map_err(|e| backend_err(format!("ONNX session builder failed: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| backend_err(format!("ONNX optimization level failed: {e}")))?
            .with_execution_providers(execution_providers(device))
            .map_err(|e| backend_err(format!("ONNX execution providers failed: {e}")))?
            .commit_from_file(path)
            .map_err(|e| {
                backend_err(format!("could not load ONNX model {}: {e}", path.display()))
            })?;

        let input = session
            .inputs()
            .first()
            .ok_or_else(|| backend_err("ONNX model has no inputs"))?;
        let output = session
            .outputs()
            .first()
            .ok_or_else(|| backend_err("ONNX model has no outputs"))?;
        let input_name = input.name().to_owned();
        let output_name = output.name().to_owned();

        // Prefer the model's own static input dims for dim_f/dim_t: input is
        // `[batch, 4, dim_f, dim_t]`. Dynamic (-1) axes keep the table value.
        if let ValueType::Tensor { shape, .. } = input.dtype() {
            if shape.len() == 4 {
                if shape[2] > 0 {
                    params.dim_f = shape[2] as usize;
                }
                if shape[3] > 0 {
                    params.dim_t = shape[3] as usize;
                }
            }
        }

        if params.dim_f == 0 || params.dim_t < 2 || params.dim_f > params.n_bins() {
            return Err(backend_err(format!(
                "invalid MDX geometry for {file_name}: n_fft={}, dim_f={}, dim_t={}",
                params.n_fft, params.dim_f, params.dim_t
            )));
        }

        let mut planner = FftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(params.n_fft);
        let inv = planner.plan_fft_inverse(params.n_fft);
        let window = hann_periodic(params.n_fft);

        Ok(Self {
            session,
            params,
            input_name,
            output_name,
            fwd,
            inv,
            window,
        })
    }

    /// Separate the model's primary stem from a 44.1 kHz planar-stereo mixture.
    /// Returns a planar-stereo waveform of the same length.
    fn separate(
        &mut self,
        mix: &PlanarStereo,
        cancel: &StemExtractCancelToken,
        mut on_segment: impl FnMut(usize, usize),
    ) -> Result<PlanarStereo, StemExtractError> {
        let p = self.params;
        let n = mix[0].len();
        let gen_len = p.gen_size();
        let trim = p.trim();
        let chunk = p.chunk_size();
        if gen_len == 0 || chunk == 0 {
            return Err(backend_err("degenerate MDX chunk geometry"));
        }

        let pad_end = (gen_len - (n % gen_len)) % gen_len;
        let padded_len = n + pad_end;
        // mixture = [trim zeros][signal][pad_end + trim zeros] per channel.
        let mut mixture: PlanarStereo = [
            vec![0.0; trim + padded_len + trim],
            vec![0.0; trim + padded_len + trim],
        ];
        for ch in 0..2 {
            mixture[ch][trim..trim + n].copy_from_slice(&mix[ch]);
        }

        let mut result: PlanarStereo = [vec![0.0; padded_len], vec![0.0; padded_len]];
        let total_segments = padded_len.div_ceil(gen_len).max(1);

        let mut chunk_l = vec![0.0f32; chunk];
        let mut chunk_r = vec![0.0f32; chunk];
        let mut seg = 0;
        let mut i = 0;
        while i < padded_len {
            if cancel.is_cancelled() {
                return Err(StemExtractError::Cancelled);
            }
            chunk_l.copy_from_slice(&mixture[0][i..i + chunk]);
            chunk_r.copy_from_slice(&mixture[1][i..i + chunk]);

            let spec = self.stft_pair(&chunk_l, &chunk_r);
            let out = self.run(spec)?;
            let (wave_l, wave_r) = self.istft_pair(&out);

            let take = gen_len.min(padded_len - i);
            result[0][i..i + take].copy_from_slice(&wave_l[trim..trim + take]);
            result[1][i..i + take].copy_from_slice(&wave_r[trim..trim + take]);

            i += gen_len;
            seg += 1;
            on_segment(seg, total_segments);
        }

        // Trim padding and apply loudness compensation.
        for ch in 0..2 {
            result[ch].truncate(n);
            for s in &mut result[ch] {
                *s *= p.compensate;
            }
        }
        Ok(result)
    }

    /// Forward STFT of a stereo chunk into the `[1, 4, dim_f, dim_t]` MDX input,
    /// with channels laid out as [L_re, L_im, R_re, R_im].
    fn stft_pair(&self, left: &[f32], right: &[f32]) -> Array4<f32> {
        let p = self.params;
        let mut arr = Array4::<f32>::zeros((1, 4, p.dim_f, p.dim_t));
        for (ch_re, ch_im, samples) in [(0usize, 1usize, left), (2usize, 3usize, right)] {
            let bins = self.stft_channel(samples);
            for t in 0..p.dim_t {
                for f in 0..p.dim_f {
                    let c = bins[t * p.dim_f + f];
                    arr[[0, ch_re, f, t]] = c.re;
                    arr[[0, ch_im, f, t]] = c.im;
                }
            }
        }
        arr
    }

    /// STFT of one channel; returns `dim_t * dim_f` complex bins (time-major).
    fn stft_channel(&self, chunk: &[f32]) -> Vec<Complex<f32>> {
        let p = self.params;
        let trim = p.trim();
        let padded = reflect_pad(chunk, trim);
        let mut out = vec![Complex::new(0.0, 0.0); p.dim_t * p.dim_f];
        let mut buf = vec![Complex::new(0.0, 0.0); p.n_fft];
        for t in 0..p.dim_t {
            let start = t * MdxParams::HOP;
            for i in 0..p.n_fft {
                buf[i] = Complex::new(padded[start + i] * self.window[i], 0.0);
            }
            self.fwd.process(&mut buf);
            let base = t * p.dim_f;
            out[base..base + p.dim_f].copy_from_slice(&buf[..p.dim_f]);
        }
        out
    }

    /// iSTFT of a `[1, 4, dim_f, dim_t]` masked spectrogram back to stereo.
    fn istft_pair(&self, data: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let p = self.params;
        let plane = p.dim_f * p.dim_t;
        // channel c occupies data[c*plane .. (c+1)*plane], indexed [f*dim_t + t].
        let left = self.istft_channel(&data[0..plane], &data[plane..2 * plane]);
        let right = self.istft_channel(&data[2 * plane..3 * plane], &data[3 * plane..4 * plane]);
        (left, right)
    }

    /// iSTFT of one channel from its real/imag planes (`[f*dim_t + t]`).
    fn istft_channel(&self, re: &[f32], im: &[f32]) -> Vec<f32> {
        let p = self.params;
        let n_fft = p.n_fft;
        let n_bins = p.n_bins();
        let out_len = n_fft + (p.dim_t - 1) * MdxParams::HOP;
        let mut acc = vec![0.0f32; out_len];
        let mut wsum = vec![0.0f32; out_len];
        let mut spec = vec![Complex::new(0.0, 0.0); n_fft];

        for t in 0..p.dim_t {
            for s in spec.iter_mut() {
                *s = Complex::new(0.0, 0.0);
            }
            for f in 0..p.dim_f.min(n_bins) {
                spec[f] = Complex::new(re[f * p.dim_t + t], im[f * p.dim_t + t]);
            }
            // Hermitian mirror for a real inverse transform.
            for k in 1..n_fft / 2 {
                spec[n_fft - k] = spec[k].conj();
            }
            spec[0].im = 0.0;
            if n_fft.is_multiple_of(2) {
                spec[n_fft / 2].im = 0.0;
            }

            self.inv.process(&mut spec);

            let start = t * MdxParams::HOP;
            let norm = 1.0 / n_fft as f32;
            for i in 0..n_fft {
                let w = self.window[i];
                acc[start + i] += spec[i].re * norm * w;
                wsum[start + i] += w * w;
            }
        }

        for (a, w) in acc.iter_mut().zip(wsum.iter()) {
            if *w > 1e-8 {
                *a /= *w;
            }
        }

        let trim = p.trim();
        acc[trim..trim + p.chunk_size()].to_vec()
    }

    /// Run one inference chunk, returning the flat `[1, 4, dim_f, dim_t]` output.
    fn run(&mut self, spec: Array4<f32>) -> Result<Vec<f32>, StemExtractError> {
        let input = Tensor::from_array((spec.shape().to_vec(), spec.into_raw_vec_and_offset().0))
            .map_err(|e| backend_err(format!("ONNX input tensor build failed: {e}")))?;
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => input])
            .map_err(|e| backend_err(format!("ONNX inference failed: {e}")))?;
        let value = outputs
            .get(self.output_name.as_str())
            .ok_or_else(|| backend_err("ONNX output missing"))?;
        let (_shape, data) = value
            .try_extract_tensor::<f32>()
            .map_err(|e| backend_err(format!("ONNX output extract failed: {e}")))?;
        Ok(data.to_vec())
    }
}

/// Real MDX-NET backend: loads installed ONNX weights and runs them per stem.
pub struct OnnxMdxBackend {
    model: StemModel,
    device: InferDevice,
    files: Vec<PathBuf>,
}

impl OnnxMdxBackend {
    pub fn new(
        model: StemModel,
        device: InferDevice,
        files: Vec<PathBuf>,
    ) -> Result<Self, StemExtractError> {
        if files.is_empty() {
            return Err(backend_err(format!(
                "no installed ONNX weights for {}",
                model.label()
            )));
        }
        Ok(Self {
            model,
            device,
            files,
        })
    }
}

impl StemInferBackend for OnnxMdxBackend {
    fn kind(&self) -> InferBackendKind {
        InferBackendKind::OnnxMdxNet
    }

    fn model(&self) -> StemModel {
        self.model
    }

    fn device(&self) -> InferDevice {
        self.device
    }

    fn separate(
        &self,
        interleaved: &[f32],
        channels: usize,
        sample_rate: u32,
        params: &StemExtractParams,
        cancel: &StemExtractCancelToken,
        on_progress: &mut dyn FnMut(StemExtractProgress),
    ) -> Result<Vec<SeparatedStem>, StemExtractError> {
        if channels == 0 || channels > 2 {
            return Err(StemExtractError::UnsupportedChannels(channels));
        }
        if interleaved.is_empty() {
            return Err(StemExtractError::EmptyInput);
        }

        on_progress(StemExtractProgress::new(
            StemExtractStage::LoadingModel,
            5.0,
            format!("Loading {} ({})", self.model.label(), self.device.label()),
        ));

        let frames = interleaved.len() / channels;
        let input_planar = deinterleave_stereo(interleaved, channels, frames);
        // MDX-NET is a 44.1 kHz model; resample the mixture in and stems out.
        let mix = resample_planar(&input_planar, sample_rate, MdxParams::SAMPLE_RATE)?;

        let requested: Vec<StemKind> = params.stems.iter().collect();
        let default_stems = self.model.default_stems();
        let single_file = self.files.len() == 1;

        // Which files must run: a file runs if its primary stem is requested, or
        // (single-file models) if the derived complementary stem is requested.
        let mut plan: Vec<(PathBuf, MdxParams, bool, Option<StemKind>)> = Vec::new();
        for path in &self.files {
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let fp = params_for_file(file_name);
            let complement = if single_file {
                default_stems.iter().copied().find(|s| *s != fp.primary)
            } else {
                None
            };
            let need_primary = requested.contains(&fp.primary);
            let need_complement = complement.is_some_and(|c| requested.contains(&c));
            if need_primary || need_complement {
                let comp = complement.filter(|_| need_complement);
                plan.push((path.clone(), fp, need_primary, comp));
            }
        }
        if plan.is_empty() {
            return Err(backend_err(format!(
                "no ONNX file in {} produces the requested stems",
                self.model.label()
            )));
        }

        let mut outputs: Vec<SeparatedStem> = Vec::new();
        let file_count = plan.len();
        for (file_index, (path, _fp, want_primary, complement)) in plan.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(StemExtractError::Cancelled);
            }
            let base = 10.0 + (file_index as f32 / file_count as f32) * 80.0;
            let span = 80.0 / file_count as f32;

            let mut model = MdxModel::load(&path, self.device)?;
            let primary = model.params.primary;
            on_progress(
                StemExtractProgress::new(
                    StemExtractStage::Separating,
                    base,
                    format!("Separating {}", primary.label()),
                )
                .with_stem(primary),
            );

            let primary_wave = model.separate(&mix, cancel, |seg, total| {
                let pct = base + (seg as f32 / total.max(1) as f32) * span;
                on_progress(
                    StemExtractProgress::new(
                        StemExtractStage::Separating,
                        pct,
                        format!("Separating {} ({seg}/{total})", primary.label()),
                    )
                    .with_stem(primary),
                );
            })?;

            if want_primary {
                let samples = finalize_stem(&primary_wave, sample_rate, channels, frames)?;
                outputs.push(SeparatedStem {
                    kind: primary,
                    samples,
                });
            }
            if let Some(comp_kind) = complement {
                // Complementary stem = mixture - primary (at 44.1 kHz), then out.
                let comp_planar: PlanarStereo = [
                    subtract(&mix[0], &primary_wave[0]),
                    subtract(&mix[1], &primary_wave[1]),
                ];
                let samples = finalize_stem(&comp_planar, sample_rate, channels, frames)?;
                outputs.push(SeparatedStem {
                    kind: comp_kind,
                    samples,
                });
            }
        }

        on_progress(StemExtractProgress::new(
            StemExtractStage::Separating,
            95.0,
            "Separation complete",
        ));
        Ok(outputs)
    }
}

/// Resample a 44.1 kHz planar stem back to the project rate and re-interleave to
/// the requested channel count, clamped to `frames`.
fn finalize_stem(
    stem_44k: &PlanarStereo,
    sample_rate: u32,
    channels: usize,
    frames: usize,
) -> Result<Vec<f32>, StemExtractError> {
    let planar = resample_planar(stem_44k, MdxParams::SAMPLE_RATE, sample_rate)?;
    Ok(interleave_stereo(&planar, channels, frames))
}

fn subtract(a: &[f32], b: &[f32]) -> Vec<f32> {
    let n = a.len().min(b.len());
    let mut out = vec![0.0; a.len()];
    for i in 0..n {
        out[i] = a[i] - b[i];
    }
    out
}

fn hann_periodic(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = std::f32::consts::PI * 2.0 * i as f32 / n as f32;
            0.5 - 0.5 * x.cos()
        })
        .collect()
}

/// Reflect-pad `signal` by `pad` samples on each side (torch STFT `center`).
fn reflect_pad(signal: &[f32], pad: usize) -> Vec<f32> {
    let n = signal.len();
    let mut out = vec![0.0f32; n + 2 * pad];
    out[pad..pad + n].copy_from_slice(signal);
    for i in 0..pad {
        // left: signal[pad], signal[pad-1] ... == mirror around index 0
        let src = (pad - i).min(n.saturating_sub(1));
        out[i] = signal[src];
        // right: mirror around the last index
        let r = n.saturating_sub(2 + i);
        out[pad + n + i] = signal[r.min(n.saturating_sub(1))];
    }
    out
}
