#pragma once

#ifdef __cplusplus
extern "C" {
#endif

void *fb_signalsmith_create(float sample_rate, int channels);
void fb_signalsmith_destroy(void *handle);
void fb_signalsmith_reset(void *handle);

// Apply the quality preset (and transpose) now. Call from a non-realtime
// thread whenever the parameters change: `process`/`output_seek` only
// reconfigure lazily when the quality differs from what is configured, and a
// preset change allocates, so doing it here keeps the audio callback free of
// that work.
int fb_signalsmith_configure(void *handle, float pitch_ratio, float quality);

// Time-stretch is expressed by the input/output sample-count ratio
// (`output_frames / input_frames`); the caller supplies exactly `input_frames`
// source samples and requests `output_frames` output samples. This keeps the
// bridge a thin, allocation-free pass-through to Signalsmith (no internal
// pending/grow buffers in the realtime path). `pitch_ratio` is the independent
// transpose factor.
int fb_signalsmith_process_stereo(
    void *handle,
    const float *input_l,
    const float *input_r,
    float *output_l,
    float *output_r,
    int input_frames,
    int output_frames,
    float pitch_ratio,
    float quality
);

int fb_signalsmith_latency_samples(void *handle);

// The two halves of that latency: `inputLatency` (input frames) and
// `outputLatency` (output frames). Either pointer may be null.
void fb_signalsmith_io_latency(void *handle, int *input_latency, int *output_latency);

// Input pre-roll length (in source frames) to feed `fb_signalsmith_output_seek`
// for a given `playback_rate` (input samples consumed per output sample, i.e.
// `1.0 / time_ratio`). Equals `inputLatency + playback_rate * outputLatency`.
int fb_signalsmith_output_seek_length(void *handle, float playback_rate);

// Prime the stretcher so the *next* `process` output starts at the first sample
// of this pre-roll — compensating the algorithmic latency. Feed the
// `input_frames` source samples starting at the intended playback position
// (length from `fb_signalsmith_output_seek_length`); the next `process` input
// continues where the pre-roll ends. Resets internally first.
int fb_signalsmith_output_seek(
    void *handle,
    const float *input_l,
    const float *input_r,
    int input_frames,
    float pitch_ratio,
    float quality
);

#ifdef __cplusplus
}
#endif
