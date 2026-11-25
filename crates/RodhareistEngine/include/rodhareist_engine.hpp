// RodhareistEngine — C++17 wrapper over the C ABI in rodhareist_engine.h.
//
// Header-only. `rodhareist::Engine` owns one engine (move-only, RAII). The
// audio-side and any-thread calls are `noexcept` and return a `Status`;
// control-side calls throw `rodhareist::Error`, carrying the engine's own
// explanation, and each has a `try…` form that returns a `Status` instead.
// The threading rules are the C header's.

#ifndef RODHAREIST_ENGINE_HPP
#define RODHAREIST_ENGINE_HPP

#include "rodhareist_engine.h"

#include <array>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace rodhareist {

enum class Status : int32_t {
    Ok = RODHAREIST_OK,
    NullPointer = RODHAREIST_NULL_POINTER,
    UnknownParam = RODHAREIST_UNKNOWN_PARAM,
    InvalidArgument = RODHAREIST_INVALID_ARGUMENT,
    BufferTooSmall = RODHAREIST_BUFFER_TOO_SMALL,
    QueueFull = RODHAREIST_QUEUE_FULL,
    ParseFailed = RODHAREIST_PARSE_FAILED,
    LoadFailed = RODHAREIST_LOAD_FAILED,
    Panic = RODHAREIST_PANIC,
    NoSuchPreset = RODHAREIST_NO_SUCH_PRESET,
    Busy = RODHAREIST_BUSY,
};

inline const char *statusName(Status status) noexcept {
    return rodhareist_status_name(static_cast<RodhareistStatus>(status));
}

class Error : public std::runtime_error {
public:
    Error(Status status, const std::string &message)
        : std::runtime_error(message), status_(status) {}
    Status status() const noexcept { return status_; }

private:
    Status status_;
};

using Meters = RodhareistMeters;
using NamInfo = RodhareistNamInfo;
using IrInfo = RodhareistIrInfo;
using Telemetry = std::array<float, RODHAREIST_TELEMETRY_SLOTS>;

struct ParamInfo {
    uint32_t index;
    std::string_view id;
    std::string_view name;
    std::string_view unit;
    float minValue;
    float maxValue;
    float defaultValue;
    uint32_t flags;

    bool described() const noexcept { return (flags & RODHAREIST_PARAM_DESCRIBED) != 0; }
    bool toggle() const noexcept { return (flags & RODHAREIST_PARAM_TOGGLE) != 0; }
    bool choice() const noexcept { return (flags & RODHAREIST_PARAM_CHOICE) != 0; }
    bool action() const noexcept { return (flags & RODHAREIST_PARAM_ACTION) != 0; }
};

struct PresetInfo {
    uint32_t index;
    std::string_view id;
    std::string_view name;
};

/// The library's ABI matches the header this was compiled against.
inline bool abiMatches() noexcept { return rodhareist_abi_version() == RODHAREIST_ABI_VERSION; }

inline std::string_view version() noexcept { return rodhareist_version(); }

/// Every wire parameter, by index. The strings live as long as the library.
inline std::vector<ParamInfo> params() {
    std::vector<ParamInfo> out;
    const uint32_t count = rodhareist_param_count();
    out.reserve(count);
    for (uint32_t i = 0; i < count; ++i) {
        RodhareistParamInfo info{};
        if (rodhareist_param_info(i, &info) == RODHAREIST_OK) {
            out.push_back({i, info.id, info.name, info.unit, info.min_value, info.max_value,
                           info.default_value, info.flags});
        }
    }
    return out;
}

/// A parameter's wire index, or nothing for an unknown id.
inline std::optional<uint32_t> paramIndex(std::string_view id) {
    const std::string text(id);
    uint32_t index = 0;
    if (rodhareist_param_index(text.c_str(), &index) != RODHAREIST_OK) {
        return std::nullopt;
    }
    return index;
}

inline std::vector<PresetInfo> presets() {
    std::vector<PresetInfo> out;
    const uint32_t count = rodhareist_preset_count();
    out.reserve(count);
    for (uint32_t i = 0; i < count; ++i) {
        RodhareistPresetInfo info{};
        if (rodhareist_preset_info(i, &info) == RODHAREIST_OK) {
            out.push_back({i, info.id, info.name});
        }
    }
    return out;
}

/// The `cab_model` value that plays the loaded impulse response.
inline uint32_t cabModelIr() noexcept { return rodhareist_cab_model_ir(); }

class Engine {
public:
    /// An engine at `sampleRate` Hz. Throws when the rate is out of range or
    /// the library is not the ABI this header describes.
    explicit Engine(double sampleRate) {
        if (!abiMatches()) {
            throw Error(Status::InvalidArgument,
                        "rodhareist_engine: the library's ABI does not match this header");
        }
        engine_ = rodhareist_engine_create(sampleRate);
        if (engine_ == nullptr) {
            throw Error(Status::InvalidArgument,
                        "rodhareist_engine: sample rate out of range (8000..384000 Hz)");
        }
    }

    ~Engine() { rodhareist_engine_destroy(engine_); }

    Engine(const Engine &) = delete;
    Engine &operator=(const Engine &) = delete;

    Engine(Engine &&other) noexcept : engine_(std::exchange(other.engine_, nullptr)) {}

    Engine &operator=(Engine &&other) noexcept {
        if (this != &other) {
            rodhareist_engine_destroy(engine_);
            engine_ = std::exchange(other.engine_, nullptr);
        }
        return *this;
    }

    /// The C handle, for calls this wrapper does not cover.
    RodhareistEngine *handle() const noexcept { return engine_; }

    double sampleRate() const noexcept { return rodhareist_engine_sample_rate(engine_); }

    // ── Audio side ───────────────────────────────────────────────────────

    /// See rodhareist_engine_process: `inRight` may be null (mono in),
    /// `outRight` null (left out only), outputs may alias inputs.
    Status process(const float *inLeft, const float *inRight, float *outLeft, float *outRight,
                   uint32_t frames) noexcept {
        return static_cast<Status>(
            rodhareist_engine_process(engine_, inLeft, inRight, outLeft, outRight, frames));
    }

    // ── Any thread ───────────────────────────────────────────────────────

    Status setTempo(double bpm) noexcept {
        return static_cast<Status>(rodhareist_engine_set_tempo(engine_, bpm));
    }

    uint32_t latencySamples() const noexcept {
        return rodhareist_engine_latency_samples(engine_);
    }

    Meters meters() const noexcept {
        Meters out{};
        rodhareist_engine_meters(engine_, &out);
        return out;
    }

    Telemetry telemetry() const noexcept {
        Telemetry out{};
        rodhareist_engine_telemetry(engine_, out.data(), out.size());
        return out;
    }

    // ── Control side ─────────────────────────────────────────────────────

    Status trySetParam(uint32_t index, float value) noexcept {
        return static_cast<Status>(rodhareist_engine_set_param(engine_, index, value));
    }

    Status trySetParam(std::string_view id, float value) {
        const std::string text(id);
        return static_cast<Status>(rodhareist_engine_set_param_by_id(engine_, text.c_str(), value));
    }

    void setParam(uint32_t index, float value) { check(trySetParam(index, value)); }
    void setParam(std::string_view id, float value) { check(trySetParam(id, value)); }

    /// The value the parameter was last set to.
    float param(uint32_t index) const {
        float value = 0.0f;
        check(static_cast<Status>(rodhareist_engine_get_param(engine_, index, &value)));
        return value;
    }

    float param(std::string_view id) const {
        const auto index = paramIndex(id);
        if (!index) {
            throw Error(Status::UnknownParam, "rodhareist_engine: no parameter " + std::string(id));
        }
        return param(*index);
    }

    void reset() { check(static_cast<Status>(rodhareist_engine_reset(engine_))); }

    /// The whole state as JSON.
    std::string state() const {
        size_t length = 0;
        rodhareist_engine_get_state(engine_, nullptr, 0, &length);
        std::string json(length + 1, '\0');
        check(static_cast<Status>(
            rodhareist_engine_get_state(engine_, json.data(), json.size(), &length)));
        json.resize(length);
        return json;
    }

    Status trySetState(std::string_view json) noexcept {
        return static_cast<Status>(rodhareist_engine_set_state(engine_, json.data(), json.size()));
    }

    void setState(std::string_view json) { check(trySetState(json)); }

    void loadPreset(uint32_t index) {
        check(static_cast<Status>(rodhareist_engine_load_preset(engine_, index)));
    }

    /// Loads a .nam capture's JSON. Set "tone_engine" to 1 to play it.
    NamInfo loadNam(std::string_view json, const std::string &name, bool stereo = false,
                    bool fullRig = false) {
        NamInfo info{};
        check(static_cast<Status>(rodhareist_engine_load_nam(
            engine_, json.data(), json.size(), name.c_str(), stereo ? 1u : 0u, fullRig ? 1u : 0u,
            &info)));
        return info;
    }

    void unloadNam() { check(static_cast<Status>(rodhareist_engine_unload_nam(engine_))); }

    /// Loads a .wav impulse response. Set "cab_model" to cabModelIr() to
    /// play it.
    IrInfo loadIr(const void *wav, size_t size, const std::string &name) {
        IrInfo info{};
        check(static_cast<Status>(rodhareist_engine_load_ir(
            engine_, static_cast<const uint8_t *>(wav), size, name.c_str(), &info)));
        return info;
    }

    IrInfo loadIr(const std::vector<uint8_t> &wav, const std::string &name) {
        return loadIr(wav.data(), wav.size(), name);
    }

    void unloadIr() { check(static_cast<Status>(rodhareist_engine_unload_ir(engine_))); }

    void collectGarbage() noexcept { rodhareist_engine_collect_garbage(engine_); }

    /// Why the last failed control call failed.
    std::string lastError() const {
        size_t length = 0;
        rodhareist_engine_last_error(engine_, nullptr, 0, &length);
        std::string text(length + 1, '\0');
        if (rodhareist_engine_last_error(engine_, text.data(), text.size(), &length) !=
            RODHAREIST_OK) {
            return {};
        }
        text.resize(length);
        return text;
    }

private:
    void check(Status status) const {
        if (status == Status::Ok) {
            return;
        }
        std::string message = lastError();
        if (message.empty()) {
            message = statusName(status);
        }
        throw Error(status, "rodhareist_engine: " + message);
    }

    RodhareistEngine *engine_ = nullptr;
};

} // namespace rodhareist

#endif // RODHAREIST_ENGINE_HPP
