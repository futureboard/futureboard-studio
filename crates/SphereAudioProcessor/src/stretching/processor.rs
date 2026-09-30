use super::error::StretchError;
use super::params::StretchParams;

pub trait StretchProcessor {
    fn reset(&mut self);
    fn set_params(&mut self, params: StretchParams);
    fn latency_samples(&self) -> usize;

    /// Pre-roll input length (in source frames) the caller must feed to
    /// [`StretchProcessor::output_seek`] to align the next output, given
    /// `playback_rate` = input samples consumed per output sample (`1.0 /
    /// time_ratio`). `0` means the backend has no latency to compensate and the
    /// caller should just [`StretchProcessor::reset`] instead. Default: `0`.
    fn seek_input_len(&self, _playback_rate: f32) -> usize {
        0
    }

    /// The backend's latency split into its two halves: `(input, output)`,
    /// where `input` is how far past a moment (in input frames) the backend
    /// must have read before it can output that moment, and `output` is how
    /// long (in output frames) the result then takes to come out. A streaming
    /// caller that wants output frame `o` to be stream position `S(o)` feeds,
    /// while producing `o`, the input up to `S(o + output) + input`.
    /// Default: `(0, 0)`.
    fn io_latency(&self) -> (usize, usize) {
        (0, 0)
    }

    /// Prime the backend so the next [`StretchProcessor::process_stereo`] output
    /// starts at the *first* frame of this pre-roll, compensating the
    /// algorithmic latency. Feed the source frames starting at the intended
    /// playback position (length from [`StretchProcessor::seek_input_len`]);
    /// the next `process_stereo` input continues from where the pre-roll ends,
    /// so the feed runs that far ahead of the output from then on (see
    /// [`StretchProcessor::io_latency`]). Resets internally first. Default:
    /// no-op (zero-latency backends).
    fn output_seek(&mut self, _input_l: &[f32], _input_r: &[f32]) {}

    /// Render `output_*.len()` output samples from `input_*.len()` source
    /// samples. For a streaming time-stretch backend the input and output
    /// lengths may differ (the time ratio is `output_len / input_len`); the
    /// caller supplies exactly the source samples to consume this block. Within
    /// a side, the left/right slices must be equal length. Backends that cannot
    /// resample (e.g. `RePitchProcessor`) document a stricter equal-length
    /// requirement.
    fn process_stereo(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        output_l: &mut [f32],
        output_r: &mut [f32],
    ) -> Result<(), StretchError>;
}
