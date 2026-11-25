// RodhareistEngine from C++: the wrapper header over the shipped library.
// Built and run by tests/cpp/run.ps1. Prints "ok" and returns 0 when every
// check holds; the first failing check prints where and returns 1.

#include "rodhareist_engine.hpp"

#include <atomic>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#define CHECK(cond)                                                              \
    do {                                                                         \
        if (!(cond)) {                                                           \
            std::fprintf(stderr, "FAILED %s:%d: %s\n", __FILE__, __LINE__, #cond); \
            return 1;                                                            \
        }                                                                        \
    } while (0)

namespace {

constexpr double kRate = 48000.0;

std::vector<float> sine(size_t frames, float level, size_t offset = 0) {
    std::vector<float> out(frames);
    for (size_t i = 0; i < frames; ++i) {
        out[i] = level * std::sin(static_cast<float>(i + offset) * 2.0f * 3.14159265f * 220.0f /
                                  static_cast<float>(kRate));
    }
    return out;
}

int run() {
    // The library and the header agree.
    CHECK(rodhareist::abiMatches());
    CHECK(!rodhareist::version().empty());
    std::printf("RodhareistEngine %s (ABI %u)\n", std::string(rodhareist::version()).c_str(),
                rodhareist_abi_version());

    // The parameter table.
    const auto params = rodhareist::params();
    CHECK(params.size() == rodhareist_param_count());
    CHECK(params.size() > 100);
    const auto power = rodhareist::paramIndex("power");
    CHECK(power.has_value());
    CHECK(params[*power].id == "power");
    CHECK(params[*power].name == "Power");
    CHECK(params[*power].toggle());
    CHECK(!rodhareist::paramIndex("no_such_param").has_value());
    const auto presets = rodhareist::presets();
    CHECK(!presets.empty());
    std::printf("%zu parameters, %zu presets, first preset %s %s\n", params.size(),
                presets.size(), std::string(presets[0].id).c_str(),
                std::string(presets[0].name).c_str());

    // A bad rate is refused.
    bool threw = false;
    try {
        rodhareist::Engine bad(1.0);
    } catch (const rodhareist::Error &error) {
        threw = error.status() == rodhareist::Status::InvalidArgument;
    }
    CHECK(threw);

    // Processing: stereo, then in place from a mono input.
    rodhareist::Engine engine(kRate);
    CHECK(engine.sampleRate() == kRate);
    auto input = sine(256, 0.3f);
    std::vector<float> left(256), right(256);
    bool sound = false;
    for (int block = 0; block < 40; ++block) {
        input = sine(256, 0.3f, static_cast<size_t>(block) * 256);
        CHECK(engine.process(input.data(), input.data(), left.data(), right.data(), 256) ==
              rodhareist::Status::Ok);
        for (float v : left) {
            CHECK(std::isfinite(v));
            sound = sound || std::fabs(v) > 1.0e-3f;
        }
    }
    CHECK(sound);
    std::vector<float> inPlace = sine(256, 0.3f);
    CHECK(engine.process(inPlace.data(), nullptr, inPlace.data(), nullptr, 256) ==
          rodhareist::Status::Ok);
    CHECK(engine.meters().in_peak > 0.1f);
    CHECK(engine.telemetry()[RODHAREIST_TELEMETRY_IN_L] > 0.1f);

    // A parameter: read back at once, heard at the next block.
    engine.setParam("power", 0.0f);
    CHECK(engine.param("power") == 0.0f);
    for (int block = 0; block < 20; ++block) {
        engine.process(input.data(), input.data(), left.data(), right.data(), 256);
    }
    CHECK(engine.latencySamples() == 0);
    for (size_t i = 0; i < 256; ++i) {
        CHECK(std::fabs(left[i] - input[i]) < 1.0e-6f);
    }
    engine.setParam("power", 1.0f);

    // State: round trip into a second engine; a bad one throws with a reason.
    engine.setParam("amp_gain", 6.5f);
    const std::string state = engine.state();
    CHECK(state.find("schema_version") != std::string::npos);
    rodhareist::Engine second(kRate);
    second.setState(state);
    CHECK(second.param("amp_gain") == 6.5f);
    CHECK(second.state() == state);
    threw = false;
    try {
        second.setState("{not json");
    } catch (const rodhareist::Error &error) {
        threw = error.status() == rodhareist::Status::ParseFailed &&
                std::strstr(error.what(), "could not be read") != nullptr;
        std::printf("bad state refused: %s\n", error.what());
    }
    CHECK(threw);
    CHECK(second.param("amp_gain") == 6.5f);
    CHECK(second.trySetParam("power", NAN) == rodhareist::Status::InvalidArgument);

    // Presets keep power.
    second.setParam("power", 0.0f);
    for (const auto &preset : presets) {
        second.loadPreset(preset.index);
        CHECK(second.param("power") == 0.0f);
    }

    // Moving an engine moves its handle.
    rodhareist::Engine moved = std::move(second);
    CHECK(moved.handle() != nullptr);
    CHECK(second.handle() == nullptr);

    // Control on one thread, audio on another.
    std::atomic<bool> audioOk{true};
    std::thread audio([&] {
        std::vector<float> in = sine(256, 0.3f), outL(256), outR(256);
        for (int block = 0; block < 400; ++block) {
            if (moved.process(in.data(), in.data(), outL.data(), outR.data(), 256) !=
                rodhareist::Status::Ok) {
                audioOk = false;
            }
            for (float v : outL) {
                if (!std::isfinite(v)) {
                    audioOk = false;
                }
            }
        }
    });
    for (int i = 0; i < 400; ++i) {
        const auto status = moved.trySetParam("amp_gain", static_cast<float>(i % 10));
        CHECK(status == rodhareist::Status::Ok || status == rodhareist::Status::QueueFull);
        if (i % 50 == 0) {
            const auto s = moved.trySetState(state);
            CHECK(s == rodhareist::Status::Ok || s == rodhareist::Status::QueueFull);
        }
        moved.setTempo(90.0 + i * 0.1);
        moved.collectGarbage();
    }
    audio.join();
    CHECK(audioOk.load());

    std::printf("ok\n");
    return 0;
}

} // namespace

int main() {
    try {
        return run();
    } catch (const std::exception &error) {
        std::fprintf(stderr, "FAILED: uncaught %s\n", error.what());
        return 1;
    }
}
