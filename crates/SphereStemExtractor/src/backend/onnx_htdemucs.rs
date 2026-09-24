//! Real HT-Demucs stem separation via ONNX Runtime (`ort`).
//!
//! Classification: scanner/offline path (worker thread only) — never call from
//! the realtime audio callback. It allocates freely and blocks on ONNX
//! inference.
//!
//! Unlike the MDX-NET family, HT-Demucs is a single-file, time-domain hybrid
//! transformer: the STFT front-/back-end lives inside the graph. The host only
//! feeds raw stereo samples and reads raw stereo stems back.
//!
//! Pipeline (per the StemSplit `htdemucs.onnx` export):
//! resample to 44.1 kHz → fixed 7.8 s (`343_980` sample) segments with 25%
//! overlap → `mix` `[1, 2, N]` model input → ONNX inference → `stems`
//! `[1, 4, 2, N]` output in `[drums, bass, other, vocals]` order → triangular
//! overlap-add → resample back to the input rate.
//!
//! GPU: the ONNX Runtime shared library is loaded dynamically (`load-dynamic`).
//! When built with `cuda` / `directml` / `coreml` and the device resolves to
//! GPU, those execution providers are tried first, then CPU.

use std::path::{Path, PathBuf};

use ndarray::Array3;
use ort::execution_providers::ExecutionProviderDispatch;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Tensor;

use super::dsp::{
    PlanarStereo, backend_err, deinterleave_stereo, interleave_stereo, resample_planar,
};
use super::{InferBackendKind, SeparatedStem, StemInferBackend};
use crate::device::InferDevice;
use crate::error::StemExtractError;
use crate::model::StemModel;
use crate::params::StemExtractParams;
use crate::progress::{StemExtractCancelToken, StemExtractProgress, StemExtractStage};
use crate::stems::StemKind;

/// HT-Demucs is trained at 44.1 kHz; input is resampled to this rate.
const SAMPLE_RATE: u32 = 44_100;
/// Fixed model segment length in samples (7.8 s · 44.1 kHz).
const N_SAMPLES: usize = 343_980;
/// Stereo channel count expected by the model.
const N_CHANNELS: usize = 2;
/// The four stems the single-file model predicts, in `stems` output order.
const SOURCES: [StemKind; 4] = [
    StemKind::Drums,
    StemKind::Bass,
    StemKind::Other,
    StemKind::Vocals,
];

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

/// Triangular overlap-add window: linear fade in/out over `overlap` samples,
/// unity in between (matches the reference `infer.py::_make_window`).
fn make_window(n: usize, overlap: usize) -> Vec<f32> {
    let mut w = vec![1.0f32; n];
    if overlap == 0 || overlap * 2 > n {
        return w;
    }
    for i in 0..overlap {
        let fade = i as f32 / (overlap - 1).max(1) as f32;
        w[i] = fade;
        w[n - overlap + i] = 1.0 - fade;
    }
    w
}

/// Real HT-Demucs backend: loads the installed single-file ONNX weights.
pub struct OnnxHtDemucsBackend {
    model: StemModel,
    device: InferDevice,
    file: PathBuf,
}

impl OnnxHtDemucsBackend {
    pub fn new(
        model: StemModel,
        device: InferDevice,
        files: Vec<PathBuf>,
    ) -> Result<Self, StemExtractError> {
        let file = files.into_iter().next().ok_or_else(|| {
            backend_err(format!("no installed ONNX weights for {}", model.label()))
        })?;
        Ok(Self {
            model,
            device,
            file,
        })
    }

    fn load_session(path: &Path, device: InferDevice) -> Result<Session, StemExtractError> {
        Session::builder()
            .map_err(|e| backend_err(format!("ONNX session builder failed: {e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| backend_err(format!("ONNX optimization level failed: {e}")))?
            .with_execution_providers(execution_providers(device))
            .map_err(|e| backend_err(format!("ONNX execution providers failed: {e}")))?
            .commit_from_file(path)
            .map_err(|e| backend_err(format!("could not load ONNX model {}: {e}", path.display())))
    }
}

impl StemInferBackend for OnnxHtDemucsBackend {
    fn kind(&self) -> InferBackendKind {
        InferBackendKind::OnnxHtDemucs
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
        // HT-Demucs is a 44.1 kHz model; resample the mixture in and stems out.
        let mix = resample_planar(&input_planar, sample_rate, SAMPLE_RATE)?;
        let total = mix[0].len();
        if total == 0 {
            return Err(StemExtractError::EmptyInput);
        }

        let requested: Vec<StemKind> = params.stems.iter().collect();
        if requested.is_empty() {
            return Err(StemExtractError::NoStemsSelected);
        }

        let mut session = Self::load_session(&self.file, self.device)?;
        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_owned())
            .ok_or_else(|| backend_err("HT-Demucs model has no inputs"))?;
        let output_name = session
            .outputs()
            .first()
            .map(|o| o.name().to_owned())
            .ok_or_else(|| backend_err("HT-Demucs model has no outputs"))?;

        let overlap = N_SAMPLES / 4;
        let stride = N_SAMPLES - overlap;
        let window = make_window(N_SAMPLES, overlap);
        let n_chunks = total.div_ceil(stride).max(1);

        // Per-source planar-stereo accumulators plus a shared window weight.
        let mut acc: Vec<PlanarStereo> = (0..SOURCES.len())
            .map(|_| [vec![0.0f32; total], vec![0.0f32; total]])
            .collect();
        let mut weight = vec![0.0f32; total];

        for chunk_index in 0..n_chunks {
            if cancel.is_cancelled() {
                return Err(StemExtractError::Cancelled);
            }
            let start = chunk_index * stride;
            if start >= total {
                break;
            }
            let end = (start + N_SAMPLES).min(total);
            let clen = end - start;

            // Build the fixed-size, zero-padded `[1, 2, N_SAMPLES]` input.
            let mut input = Array3::<f32>::zeros((1, N_CHANNELS, N_SAMPLES));
            for ch in 0..N_CHANNELS {
                let src = &mix[ch][start..end];
                for (i, &s) in src.iter().enumerate() {
                    input[[0, ch, i]] = s;
                }
            }

            let base = 10.0 + (chunk_index as f32 / n_chunks as f32) * 80.0;
            on_progress(StemExtractProgress::new(
                StemExtractStage::Separating,
                base,
                format!("Separating segment {}/{n_chunks}", chunk_index + 1),
            ));

            let tensor = Tensor::from_array((input.shape().to_vec(), input.into_raw_vec_and_offset().0))
                .map_err(|e| backend_err(format!("ONNX input tensor build failed: {e}")))?;
            let outputs = session
                .run(ort::inputs![input_name.as_str() => tensor])
                .map_err(|e| backend_err(format!("ONNX inference failed: {e}")))?;
            let value = outputs
                .get(output_name.as_str())
                .ok_or_else(|| backend_err("HT-Demucs output missing"))?;
            let (_shape, data) = value
                .try_extract_tensor::<f32>()
                .map_err(|e| backend_err(format!("ONNX output extract failed: {e}")))?;

            // Output is `[1, 4, 2, seg]`; derive the per-channel segment length
            // from the flat data so we do not depend on the shape's exact type.
            let planes = SOURCES.len() * N_CHANNELS;
            if data.is_empty() || !data.len().is_multiple_of(planes) {
                return Err(backend_err(format!(
                    "unexpected HT-Demucs output length {}",
                    data.len()
                )));
            }
            let seg = data.len() / planes;
            if seg < clen {
                return Err(backend_err(format!(
                    "HT-Demucs segment {seg} shorter than chunk {clen}"
                )));
            }

            // Overlap-add each source/channel with the triangular window.
            for (s, source_acc) in acc.iter_mut().enumerate() {
                for ch in 0..N_CHANNELS {
                    let plane = (s * N_CHANNELS + ch) * seg;
                    let dst = &mut source_acc[ch][start..end];
                    for (k, dst_sample) in dst.iter_mut().enumerate().take(clen) {
                        *dst_sample += data[plane + k] * window[k];
                    }
                }
            }
            for (k, w) in window.iter().enumerate().take(clen) {
                weight[start + k] += *w;
            }
        }

        // Normalize by accumulated window weight.
        for source_acc in acc.iter_mut() {
            for ch in 0..N_CHANNELS {
                for (sample, w) in source_acc[ch].iter_mut().zip(weight.iter()) {
                    *sample /= w.max(1e-8);
                }
            }
        }

        // Emit the requested stems in the model's natural source order.
        let mut outputs: Vec<SeparatedStem> = Vec::new();
        for (s, kind) in SOURCES.iter().enumerate() {
            if !requested.contains(kind) {
                continue;
            }
            let planar = resample_planar(&acc[s], SAMPLE_RATE, sample_rate)?;
            let samples = interleave_stereo(&planar, channels, frames);
            outputs.push(SeparatedStem {
                kind: *kind,
                samples,
            });
        }

        if outputs.is_empty() {
            return Err(backend_err(format!(
                "no requested stem is produced by {}",
                self.model.label()
            )));
        }

        on_progress(StemExtractProgress::new(
            StemExtractStage::Separating,
            95.0,
            "Separation complete",
        ));
        Ok(outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_fades_in_and_out() {
        let w = make_window(16, 4);
        assert_eq!(w.len(), 16);
        assert!((w[0] - 0.0).abs() < 1e-6);
        assert!((w[3] - 1.0).abs() < 1e-6);
        assert!((w[8] - 1.0).abs() < 1e-6);
        assert!((w[15] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn sources_match_model_output_order() {
        assert_eq!(SOURCES.len(), 4);
        assert_eq!(SOURCES[0], StemKind::Drums);
        assert_eq!(SOURCES[3], StemKind::Vocals);
    }
}
