#include "signalsmith_bridge.h"

#include <algorithm>
#include <cmath>
#include <cstring>
#include <vector>

#include "signalsmith-stretch.h"

namespace {

using Stretch = signalsmith::stretch::SignalsmithStretch<float>;

constexpr int kErrorNull = -1;
constexpr int kErrorInvalid = -2;
constexpr int kErrorProcess = -3;

bool valid_ratio(float value) {
    return std::isfinite(value) && value > 0.0f;
}

template <typename T>
struct ChannelInput {
    const T *channels[2];
    int length;

    const T *operator[](int channel) const {
        return channels[channel];
    }
};

template <typename T>
struct ChannelOutput {
    T *channels[2];
    int length;

    T *operator[](int channel) {
        return channels[channel];
    }
};

struct FbSignalsmithHandle {
    Stretch stretch;
    int channels = 2;
    float sample_rate = 48'000.0f;
    float pitch_ratio = 1.0f;
    float quality = 0.75f;
    bool configured = false;
    // Reused mono down-mix scratch (grows only; no per-block allocation in the
    // steady state, so the realtime callback stays allocation-free).
    std::vector<float> mono_in;
    std::vector<float> mono_out;

    void apply_preset() {
        if (channels <= 0 || !std::isfinite(sample_rate) || sample_rate <= 0.0f) {
            configured = false;
            return;
        }

        if (quality < 0.5f) {
            stretch.presetCheaper(channels, sample_rate, true);
        } else {
            stretch.presetDefault(channels, sample_rate, false);
        }
        configured = true;
    }

    void apply_ratios() {
        if (!configured) {
            apply_preset();
        }
        stretch.setTransposeFactor(valid_ratio(pitch_ratio) ? pitch_ratio : 1.0f);
    }
};

} // namespace

// Rust declares every function below `extern "C"`, not `"C-unwind"`: an
// exception reaching it is undefined behaviour. The stretcher allocates when
// it is (re)configured, so each body runs inside `try`, and an out-of-memory
// `std::bad_alloc` becomes an error code (or a null handle) instead.
extern "C" {

void *fb_signalsmith_create(float sample_rate, int channels) {
    if (!std::isfinite(sample_rate) || sample_rate <= 0.0f || channels <= 0 || channels > 2) {
        return nullptr;
    }

    FbSignalsmithHandle *handle = nullptr;
    try {
        // `std::nothrow` covers the allocation only; the stretcher's own
        // constructor and presets can still throw.
        handle = new FbSignalsmithHandle();
        handle->sample_rate = sample_rate;
        handle->channels = channels;
        handle->apply_preset();
        handle->apply_ratios();
        return handle;
    } catch (...) {
        delete handle;
        return nullptr;
    }
}

void fb_signalsmith_destroy(void *handle) {
    delete static_cast<FbSignalsmithHandle *>(handle);
}

void fb_signalsmith_reset(void *handle) {
    if (handle == nullptr) {
        return;
    }
    auto *state = static_cast<FbSignalsmithHandle *>(handle);
    try {
        state->stretch.reset();
    } catch (...) {
    }
}

int fb_signalsmith_configure(void *handle, float pitch_ratio, float quality) {
    if (handle == nullptr) {
        return kErrorNull;
    }
    auto *state = static_cast<FbSignalsmithHandle *>(handle);
    if (!valid_ratio(pitch_ratio)) {
        pitch_ratio = 1.0f;
    }
    if (!std::isfinite(quality)) {
        quality = 0.75f;
    }
    try {
        const bool reconfigure = !state->configured || state->quality != quality;
        state->pitch_ratio = pitch_ratio;
        state->quality = quality;
        if (reconfigure) {
            state->apply_preset();
        }
        state->apply_ratios();
        return 0;
    } catch (...) {
        return kErrorProcess;
    }
}

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
) {
    if (handle == nullptr || input_l == nullptr || input_r == nullptr || output_l == nullptr
        || output_r == nullptr || input_frames <= 0 || output_frames <= 0) {
        return kErrorNull;
    }

    auto *state = static_cast<FbSignalsmithHandle *>(handle);
    if (!valid_ratio(pitch_ratio)) {
        pitch_ratio = 1.0f;
    }
    if (!std::isfinite(quality)) {
        quality = 0.75f;
    }

    // Direct, allocation-free pass-through: the caller supplies exactly the
    // input samples the stretcher should consume to produce `output_frames`.
    // The time-stretch ratio is `output_frames / input_frames`.
    try {
        const bool reconfigure = !state->configured || state->quality != quality;
        state->pitch_ratio = pitch_ratio;
        state->quality = quality;

        if (reconfigure) {
            state->apply_preset();
        }
        state->apply_ratios();

        if (state->channels == 1) {
            if (static_cast<int>(state->mono_in.size()) < input_frames) {
                state->mono_in.resize(static_cast<size_t>(input_frames));
            }
            if (static_cast<int>(state->mono_out.size()) < output_frames) {
                state->mono_out.resize(static_cast<size_t>(output_frames));
            }
            for (int i = 0; i < input_frames; ++i) {
                state->mono_in[static_cast<size_t>(i)] = 0.5f * (input_l[i] + input_r[i]);
            }

            ChannelInput<float> mono_in_io{ { state->mono_in.data(), state->mono_in.data() },
                                            input_frames };
            ChannelOutput<float> mono_out_io{ { state->mono_out.data(), state->mono_out.data() },
                                              output_frames };
            state->stretch.process(mono_in_io, input_frames, mono_out_io, output_frames);

            for (int i = 0; i < output_frames; ++i) {
                const float sample = state->mono_out[static_cast<size_t>(i)];
                output_l[i] = sample;
                output_r[i] = sample;
            }
        } else {
            ChannelInput<float> inputs{ { input_l, input_r }, input_frames };
            ChannelOutput<float> outputs{ { output_l, output_r }, output_frames };
            state->stretch.process(inputs, input_frames, outputs, output_frames);
        }
        return 0;
    } catch (...) {
        return kErrorProcess;
    }
}

int fb_signalsmith_latency_samples(void *handle) {
    if (handle == nullptr) {
        return 0;
    }
    auto *state = static_cast<FbSignalsmithHandle *>(handle);
    try {
        return state->stretch.inputLatency() + state->stretch.outputLatency();
    } catch (...) {
        return 0;
    }
}

void fb_signalsmith_io_latency(void *handle, int *input_latency, int *output_latency) {
    int in = 0;
    int out = 0;
    if (handle != nullptr) {
        auto *state = static_cast<FbSignalsmithHandle *>(handle);
        try {
            if (!state->configured) {
                state->apply_preset();
            }
            in = state->stretch.inputLatency();
            out = state->stretch.outputLatency();
        } catch (...) {
            in = 0;
            out = 0;
        }
    }
    if (input_latency != nullptr) {
        *input_latency = in > 0 ? in : 0;
    }
    if (output_latency != nullptr) {
        *output_latency = out > 0 ? out : 0;
    }
}

int fb_signalsmith_output_seek_length(void *handle, float playback_rate) {
    if (handle == nullptr) {
        return 0;
    }
    auto *state = static_cast<FbSignalsmithHandle *>(handle);
    if (!valid_ratio(playback_rate)) {
        playback_rate = 1.0f;
    }
    try {
        if (!state->configured) {
            state->apply_preset();
        }
        // inputLatency + playback_rate * outputLatency (see outputSeekLength()).
        const int len = static_cast<int>(state->stretch.outputSeekLength(playback_rate));
        return len > 0 ? len : 0;
    } catch (...) {
        return 0;
    }
}

int fb_signalsmith_output_seek(
    void *handle,
    const float *input_l,
    const float *input_r,
    int input_frames,
    float pitch_ratio,
    float quality
) {
    if (handle == nullptr || input_l == nullptr || input_r == nullptr || input_frames <= 0) {
        return kErrorNull;
    }

    auto *state = static_cast<FbSignalsmithHandle *>(handle);
    if (!valid_ratio(pitch_ratio)) {
        pitch_ratio = 1.0f;
    }
    if (!std::isfinite(quality)) {
        quality = 0.75f;
    }

    try {
        const bool reconfigure = !state->configured || state->quality != quality;
        state->pitch_ratio = pitch_ratio;
        state->quality = quality;
        if (reconfigure) {
            state->apply_preset();
        }
        state->apply_ratios();

        if (state->channels == 1) {
            if (static_cast<int>(state->mono_in.size()) < input_frames) {
                state->mono_in.resize(static_cast<size_t>(input_frames));
            }
            for (int i = 0; i < input_frames; ++i) {
                state->mono_in[static_cast<size_t>(i)] = 0.5f * (input_l[i] + input_r[i]);
            }
            ChannelInput<float> mono_in_io{ { state->mono_in.data(), state->mono_in.data() },
                                            input_frames };
            state->stretch.outputSeek(mono_in_io, input_frames);
        } else {
            ChannelInput<float> inputs{ { input_l, input_r }, input_frames };
            state->stretch.outputSeek(inputs, input_frames);
        }
        return 0;
    } catch (...) {
        return kErrorProcess;
    }
}

} // extern "C"
