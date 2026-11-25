// Console check of the visualizer link: connects like the app does, prints
// what Studio publishes, asks for one track's audio, and exits non-zero when
// any part of the link does not work. No window, no SDL.
//
//   FutureboardVisualizerProbe [--link NAME] [--track N] [--seconds S]

#include "link/FutureboardLink.hpp"

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

namespace {

double db(float linear) { return linear > 1e-10f ? 20.0 * std::log10(linear) : -200.0; }

} // namespace

int main(int argc, char** argv) {
    std::string name = fblink::kRegionName;
    int trackIndex = 1; // 0-based: the second track by default
    double seconds = 1.5;
    for (int i = 1; i + 1 < argc; ++i) {
        if (!std::strcmp(argv[i], "--link")) name = argv[++i];
        else if (!std::strcmp(argv[i], "--track")) trackIndex = std::max(0, std::atoi(argv[++i]) - 1);
        else if (!std::strcmp(argv[i], "--seconds")) seconds = std::max(0.2, std::atof(argv[++i]));
    }

    fblink::Link link;
    if (!link.open(name)) {
        std::printf("FAIL open %s: %s\n", name.c_str(), link.lastError().c_str());
        return 2;
    }
    if (!link.writerAlive(fblink::nowMs())) {
        std::printf("FAIL the writer's heartbeat is stale\n");
        return 3;
    }
    std::printf("link     %s, writer pid %u\n", name.c_str(), link.writerPid());

    std::vector<float> l(8192), r(8192);
    auto pump = [&](double secs, bool requestTrack, uint64_t key, size_t& masterFrames, size_t& trackFrames,
                    float& trackPeak) {
        const auto until = std::chrono::steady_clock::now() + std::chrono::duration<double>(secs);
        while (std::chrono::steady_clock::now() < until) {
            link.stampHeartbeat();
            if (requestTrack) link.requestTrack(key);
            uint64_t skipped = 0;
            size_t n;
            while ((n = link.readMaster(l.data(), r.data(), l.size(), skipped)) > 0) masterFrames += n;
            while ((n = link.readTrack(l.data(), r.data(), l.size(), skipped)) > 0) {
                if (link.tappedTrackKey() != key) continue;
                trackFrames += n;
                for (size_t i = 0; i < n; ++i) trackPeak = std::max(trackPeak, std::max(std::fabs(l[i]), std::fabs(r[i])));
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
    };

    // Studio publishes only once it sees a reader: wait for frames.
    size_t masterFrames = 0, trackFrames = 0;
    float trackPeak = 0.0f;
    pump(seconds, false, 0, masterFrames, trackFrames, trackPeak);
    fblink::Frame frame{};
    std::vector<fblink::TrackEntry> tracks;
    if (!link.snapshot(frame, tracks) || frame.counter == 0) {
        std::printf("FAIL no frame published while reading\n");
        return 4;
    }
    const uint64_t firstCounter = frame.counter;
    std::printf("project  %s\n", fblink::nameOf(frame.project_name, sizeof frame.project_name).c_str());
    std::printf("frame    #%llu  %.2f BPM %u/%u  beat %.2f  %s%s%s\n",
                static_cast<unsigned long long>(frame.counter), frame.bpm, frame.time_sig_num, frame.time_sig_den,
                frame.position_beats, (frame.transport & fblink::transport::Playing) ? "PLAY " : "STOP ",
                (frame.transport & fblink::transport::Recording) ? "REC " : "",
                (frame.transport & fblink::transport::Looping) ? "LOOP" : "");
    std::printf("master   peak %.1f / %.1f dBFS, fader %+.1f dB\n", db(frame.master.peak_l), db(frame.master.peak_r),
                frame.master_volume_db);
    std::printf("tracks   %zu\n", tracks.size());
    for (size_t i = 0; i < tracks.size() && i < 8; ++i) {
        const auto& t = tracks[i];
        std::printf("  %2zu %-20s kind %u depth %u flags %02x  peak %6.1f dB  fader %+5.1f  pan %+.2f\n", i + 1,
                    fblink::nameOf(t.name, sizeof t.name).c_str(), t.kind, t.depth, t.flags,
                    db(std::max(t.levels.peak_l, t.levels.peak_r)), t.volume_db, t.pan);
    }
    std::printf("audio    %u Hz, %zu master frames in %.1f s\n", link.sampleRate(), masterFrames, seconds);

    int failures = 0;
    if (tracks.empty()) {
        std::printf("note     no tracks to tap\n");
    } else {
        const auto& target = tracks[std::min(static_cast<size_t>(trackIndex), tracks.size() - 1)];
        trackFrames = 0;
        pump(seconds, true, target.key, masterFrames, trackFrames, trackPeak);
        const bool tapped = link.tappedTrackKey() == target.key;
        std::printf("track    %s: %s, %zu frames, peak %.1f dBFS\n",
                    fblink::nameOf(target.name, sizeof target.name).c_str(), tapped ? "tapped" : "NOT tapped",
                    trackFrames, db(trackPeak));
        if (!tapped) {
            std::printf("FAIL Studio did not tap the requested track\n");
            ++failures;
        }
        link.requestTrack(0);
    }
    fblink::Frame later{};
    if (link.snapshot(later, tracks) && later.counter <= firstCounter) {
        std::printf("FAIL frames stopped advancing\n");
        ++failures;
    }
    if (failures == 0) std::printf("ok\n");
    return failures == 0 ? 0 : 5;
}
