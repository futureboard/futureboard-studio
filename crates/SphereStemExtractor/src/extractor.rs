use crate::backend::{InferBackendKind, SeparatedStem, create_mdx_net_backend};
use crate::device::{StemPlatformRuntime, resolve_current_platform_runtime};
use crate::error::StemExtractError;
use crate::params::StemExtractParams;
use crate::progress::{StemExtractCancelToken, StemExtractProgress, StemExtractStage};
use crate::stems::StemKind;

/// Interleaved PCM input for offline stem extraction.
#[derive(Clone, Debug)]
pub struct StemExtractInput {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

impl StemExtractInput {
    pub fn new(sample_rate: u32, channels: usize, samples: Vec<f32>) -> Self {
        Self {
            sample_rate,
            channels,
            samples,
        }
    }

    pub fn frames(&self) -> usize {
        self.samples.len().checked_div(self.channels).unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
pub struct StemExtractOutput {
    pub kind: StemKind,
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct StemExtractResult {
    pub model: crate::model::StemModel,
    pub device: crate::device::InferDevice,
    pub runtime: StemPlatformRuntime,
    pub backend: InferBackendKind,
    pub stems: Vec<StemExtractOutput>,
}

/// Run offline stem extraction on interleaved PCM.
///
/// Safe for worker threads only — never call from the realtime audio callback.
pub fn extract_stems(
    input: &StemExtractInput,
    params: &StemExtractParams,
    cancel: &StemExtractCancelToken,
    mut on_progress: impl FnMut(StemExtractProgress),
) -> Result<StemExtractResult, StemExtractError> {
    params.validate()?;
    if input.sample_rate == 0 {
        return Err(StemExtractError::InvalidSampleRate);
    }
    if input.channels == 0 || input.channels > 2 {
        return Err(StemExtractError::UnsupportedChannels(input.channels));
    }
    if input.samples.is_empty() || input.frames() == 0 {
        return Err(StemExtractError::EmptyInput);
    }
    if cancel.is_cancelled() {
        return Err(StemExtractError::Cancelled);
    }

    on_progress(StemExtractProgress::new(
        StemExtractStage::Preparing,
        1.0,
        format!(
            "Preparing {} ({})",
            params.model.label(),
            params.device.label()
        ),
    ));

    let runtime = resolve_current_platform_runtime()?;
    let device = runtime.device();
    let mut effective = params.clone();
    effective.device = device;

    let backend = create_mdx_net_backend(effective.model, device)?;
    let separated = backend.separate(
        &input.samples,
        input.channels,
        input.sample_rate,
        &effective,
        cancel,
        &mut on_progress,
    )?;

    if cancel.is_cancelled() {
        return Err(StemExtractError::Cancelled);
    }

    on_progress(StemExtractProgress::new(
        StemExtractStage::Complete,
        100.0,
        format!("Extracted {} stem(s)", separated.len()),
    ));

    Ok(StemExtractResult {
        model: effective.model,
        device,
        runtime,
        backend: backend.kind(),
        stems: separated
            .into_iter()
            .map(|SeparatedStem { kind, samples }| StemExtractOutput {
                kind,
                sample_rate: input.sample_rate,
                channels: input.channels,
                samples,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{InferDevice, StemPlatformRuntime};
    use crate::model::StemModel;
    use crate::params::StemExtractParams;

    #[test]
    fn extract_mdx_net_uses_the_current_platform_runtime() {
        let input = StemExtractInput::new(48_000, 2, vec![0.1; 48_000 * 2]);
        let params = StemExtractParams::mdx_net_cpu();
        let result =
            extract_stems(&input, &params, &StemExtractCancelToken::new(), |_| {}).unwrap();
        assert_eq!(result.model, StemModel::MdxNet);
        assert_eq!(result.runtime, StemPlatformRuntime::CoreMl);
        assert_eq!(result.device, InferDevice::Gpu);
        assert_eq!(result.stems.len(), 4);
    }

    #[test]
    fn cancel_before_start_errors() {
        let input = StemExtractInput::new(48_000, 1, vec![0.1; 128]);
        let cancel = StemExtractCancelToken::new();
        cancel.cancel();
        let err =
            extract_stems(&input, &StemExtractParams::default(), &cancel, |_| {}).unwrap_err();
        assert_eq!(err, StemExtractError::Cancelled);
    }
}
