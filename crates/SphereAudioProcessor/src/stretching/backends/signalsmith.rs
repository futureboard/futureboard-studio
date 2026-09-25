use std::ptr::NonNull;

use crate::stretching::error::StretchError;
use crate::stretching::params::StretchParams;
use crate::stretching::processor::StretchProcessor;
use crate::stretching::ratios::effective_pitch_ratio;

#[link(name = "sphere_signalsmith_bridge", kind = "static")]
unsafe extern "C" {
    fn fb_signalsmith_create(sample_rate: f32, channels: i32) -> *mut std::ffi::c_void;
    fn fb_signalsmith_destroy(handle: *mut std::ffi::c_void);
    fn fb_signalsmith_reset(handle: *mut std::ffi::c_void);
    fn fb_signalsmith_configure(
        handle: *mut std::ffi::c_void,
        pitch_ratio: f32,
        quality: f32,
    ) -> i32;
    fn fb_signalsmith_process_stereo(
        handle: *mut std::ffi::c_void,
        input_l: *const f32,
        input_r: *const f32,
        output_l: *mut f32,
        output_r: *mut f32,
        input_frames: i32,
        output_frames: i32,
        pitch_ratio: f32,
        quality: f32,
    ) -> i32;
    fn fb_signalsmith_latency_samples(handle: *mut std::ffi::c_void) -> i32;
    fn fb_signalsmith_output_seek_length(handle: *mut std::ffi::c_void, playback_rate: f32) -> i32;
    fn fb_signalsmith_output_seek(
        handle: *mut std::ffi::c_void,
        input_l: *const f32,
        input_r: *const f32,
        input_frames: i32,
        pitch_ratio: f32,
        quality: f32,
    ) -> i32;
}

pub struct SignalsmithProcessor {
    handle: NonNull<std::ffi::c_void>,
    _channels: usize,
    params: StretchParams,
}

impl SignalsmithProcessor {
    pub fn new(sample_rate: f32, channels: usize) -> Result<Self, StretchError> {
        let handle = unsafe { fb_signalsmith_create(sample_rate, channels as i32) };
        let handle = NonNull::new(handle).ok_or(StretchError::BackendUnavailable(
            super::super::params::StretchBackend::Signalsmith,
        ))?;

        Ok(Self {
            handle,
            _channels: channels,
            params: StretchParams::default(),
        })
    }

    fn pitch_ratio(&self) -> f32 {
        effective_pitch_ratio(&self.params)
    }
}

impl Drop for SignalsmithProcessor {
    fn drop(&mut self) {
        unsafe {
            fb_signalsmith_destroy(self.handle.as_ptr());
        }
    }
}

impl StretchProcessor for SignalsmithProcessor {
    fn reset(&mut self) {
        unsafe {
            fb_signalsmith_reset(self.handle.as_ptr());
        }
    }

    /// Control-thread only. Applies the preset here so the first
    /// `output_seek`/`process_stereo` in the audio callback finds the
    /// stretcher already configured for this quality and does not rebuild
    /// (and allocate) it on the realtime thread.
    fn set_params(&mut self, params: StretchParams) {
        self.params = params.sanitized();
        unsafe {
            fb_signalsmith_configure(
                self.handle.as_ptr(),
                self.pitch_ratio(),
                self.params.quality,
            );
        }
    }

    fn latency_samples(&self) -> usize {
        let latency = unsafe { fb_signalsmith_latency_samples(self.handle.as_ptr()) };
        latency.max(0) as usize
    }

    fn seek_input_len(&self, playback_rate: f32) -> usize {
        let rate = if playback_rate.is_finite() && playback_rate > 0.0 {
            playback_rate.clamp(0.05, 20.0)
        } else {
            1.0
        };
        let len = unsafe { fb_signalsmith_output_seek_length(self.handle.as_ptr(), rate) };
        // A malformed bridge result must not turn into a huge realtime scratch
        // allocation. At 192 kHz the default preset is still well below this.
        len.clamp(0, 262_144) as usize
    }

    fn output_seek(&mut self, input_l: &[f32], input_r: &[f32]) {
        let frames = input_l.len().min(input_r.len());
        let Ok(frames_i32) = i32::try_from(frames) else {
            return;
        };
        if frames_i32 <= 0 {
            return;
        }
        unsafe {
            fb_signalsmith_output_seek(
                self.handle.as_ptr(),
                input_l.as_ptr(),
                input_r.as_ptr(),
                frames_i32,
                self.pitch_ratio(),
                self.params.quality,
            );
        }
    }

    /// Time-stretch by `output.len() / input.len()` and pitch-shift by the
    /// param transpose factor. Input and output lengths may differ; the caller
    /// supplies exactly the source samples to consume (see [`StretchProcessor`]).
    fn process_stereo(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        output_l: &mut [f32],
        output_r: &mut [f32],
    ) -> Result<(), StretchError> {
        if input_l.len() != input_r.len() || output_l.len() != output_r.len() {
            return Err(StretchError::BufferLengthMismatch);
        }
        if input_l.is_empty() || output_l.is_empty() {
            return Err(StretchError::BufferLengthMismatch);
        }

        let input_frames = i32::try_from(input_l.len())
            .map_err(|_| StretchError::InvalidParams("frame count exceeds i32::MAX".to_string()))?;
        let output_frames = i32::try_from(output_l.len())
            .map_err(|_| StretchError::InvalidParams("frame count exceeds i32::MAX".to_string()))?;

        let status = unsafe {
            fb_signalsmith_process_stereo(
                self.handle.as_ptr(),
                input_l.as_ptr(),
                input_r.as_ptr(),
                output_l.as_mut_ptr(),
                output_r.as_mut_ptr(),
                input_frames,
                output_frames,
                self.pitch_ratio(),
                self.params.quality,
            )
        };

        if status == 0 {
            // Native DSP is allowed to fail closed. Never let a NaN/Inf from a
            // third-party kernel contaminate the rest of the graph.
            for sample in output_l.iter_mut().chain(output_r.iter_mut()) {
                if !sample.is_finite() {
                    *sample = 0.0;
                }
            }
            Ok(())
        } else {
            Err(StretchError::BackendFailed(format!(
                "signalsmith process_stereo failed with code {status}"
            )))
        }
    }
}

unsafe impl Send for SignalsmithProcessor {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stretching::factory::signalsmith_stretch_available;

    /// After `output_seek` pre-roll priming, the *first* `process` output already
    /// reflects the input level — i.e. the algorithmic latency is compensated and
    /// the output is aligned to the playback position. Without priming the same
    /// first block is still inside the latency ramp and is heavily attenuated.
    #[test]
    fn output_seek_compensates_latency() {
        if !signalsmith_stretch_available() {
            return; // C++ backend not built in this environment.
        }

        let block = 1024usize;
        let make = || {
            let mut p = SignalsmithProcessor::new(48_000.0, 2).expect("create");
            p.set_params(StretchParams::default()); // pitch 1.0, identity transpose
            p
        };
        let head = |out: &[f32]| out[..64].iter().sum::<f32>() / 64.0;

        // Unprimed: reset, then process a DC=1.0 block. The first samples are the
        // latency region flushing out → near silence.
        let mut unprimed = make();
        unprimed.reset();
        let input = vec![1.0_f32; block];
        let (mut ul, mut ur) = (vec![0.0_f32; block], vec![0.0_f32; block]);
        unprimed
            .process_stereo(&input, &input, &mut ul, &mut ur)
            .expect("process");
        let unprimed_head = head(&ul);

        // Primed: feed `seek_input_len` of DC=1.0 history via output_seek, then the
        // same block. The head should already sit near the input level.
        let mut primed = make();
        let seek_len = primed.seek_input_len(1.0);
        assert!(seek_len > 0, "expected non-zero latency pre-roll");
        let prime = vec![1.0_f32; seek_len];
        primed.output_seek(&prime, &prime);
        let (mut pl, mut pr) = (vec![0.0_f32; block], vec![0.0_f32; block]);
        primed
            .process_stereo(&input, &input, &mut pl, &mut pr)
            .expect("process");
        let primed_head = head(&pl);

        assert!(
            primed_head > unprimed_head + 0.25,
            "priming should align output (primed head {primed_head} should exceed \
             unprimed head {unprimed_head})"
        );
    }
}
