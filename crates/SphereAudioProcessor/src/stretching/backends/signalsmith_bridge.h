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

// Input pre-roll length (in source frames) to feed `fb_signalsmith_output_seek`
// for a given `playback_rate` (input samples consumed per output sample, i.e.
// `1.0 / time_ratio`). Equals `inputLatency + playback_rate * outputLatency`.
int fb_signalsmith_output_seek_length(void *handle, float playback_rate);

// Prime the stretcher so the *next* `process` output is aligned to the sample
// immediately after this pre-roll — compensating the algorithmic latency. Feed
// the `input_frames` source samples ending at the intended playback position
// (length from `fb_signalsmith_output_seek_length`). Resets internally first.
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
