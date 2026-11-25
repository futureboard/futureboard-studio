#pragma once
// Reader side of the Futureboard visualizer link: the shared-memory region
// Futureboard Studio publishes (Rust: crates/VisualizerLink, which documents
// the protocol). Layouts here must match it byte for byte; the static_asserts
// pin every offset the app reads.

#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>

namespace fblink {

constexpr uint32_t kMagic = 0x5A564246u; // "FBVZ" little-endian
constexpr uint32_t kVersion = 1;
constexpr size_t kMaxTracks = 512;
constexpr size_t kNameBytes = 48;
constexpr size_t kProjectNameBytes = 64;
constexpr size_t kAudioFrames = size_t{1} << 16;
constexpr uint64_t kReaderTimeoutMs = 2000;
constexpr uint64_t kWriterTimeoutMs = 3000;

#ifdef _WIN32
constexpr const char* kRegionName = "Local\\FutureboardVisualizer.1";
#else
constexpr const char* kRegionName = "/fb_visualizer_1";
#endif

namespace kind {
constexpr uint8_t Audio = 0, Midi = 1, Instrument = 2, Bus = 3, Return = 4, Group = 5, Video = 6,
                  PluginOutput = 7;
}
namespace flags {
constexpr uint8_t Muted = 1, Solo = 2, Armed = 4, Selected = 8, Clipped = 16;
}
namespace transport {
constexpr uint32_t Playing = 1, Recording = 2, Looping = 4;
}

struct Header {
    uint32_t magic, version, total_size, header_size, max_tracks, track_entry_size, audio_frames;
    uint32_t sample_rate; // atomic
    uint32_t writer_pid, frame_offset, tracks_offset, master_ring_offset, track_ring_offset, reserved0;
    uint64_t writer_heartbeat_ms; // atomic
    uint64_t reader_heartbeat_ms; // atomic
    uint64_t requested_track_key; // atomic
    uint64_t tapped_track_key;    // atomic
    uint32_t seq;                 // atomic, seqlock over Frame + tracks
    uint32_t reserved1[9];
};

struct Levels {
    float peak_l, peak_r, rms_l, rms_r;
};

struct Frame {
    uint64_t counter;
    double position_beats;
    double position_seconds;
    float bpm;
    uint32_t time_sig_num, time_sig_den;
    uint32_t transport;
    Levels master;
    Levels monitor;
    float master_volume_db;
    uint32_t track_count;
    uint64_t selected_track_key;
    char project_name[kProjectNameBytes];
    uint8_t reserved[104];
};

struct TrackEntry {
    uint64_t key;
    uint8_t kind, flags, depth, pad;
    uint32_t color; // 0xRRGGBB
    float volume_db;
    float pan;
    Levels levels;
    char name[kNameBytes];
    uint8_t reserved[40];
};

constexpr size_t kHeaderSize = 128;
constexpr size_t kFrameOffset = kHeaderSize;
constexpr size_t kTracksOffset = kFrameOffset + 256;
constexpr size_t kRingHeadSize = 64;
constexpr size_t kRingSize = kRingHeadSize + 2 * 4 * kAudioFrames;
constexpr size_t kMasterRingOffset = kTracksOffset + kMaxTracks * 128;
constexpr size_t kTrackRingOffset = kMasterRingOffset + kRingSize;
constexpr size_t kTotalSize = kTrackRingOffset + kRingSize;

static_assert(sizeof(Header) == kHeaderSize);
static_assert(offsetof(Header, sample_rate) == 28);
static_assert(offsetof(Header, writer_heartbeat_ms) == 56);
static_assert(offsetof(Header, reader_heartbeat_ms) == 64);
static_assert(offsetof(Header, requested_track_key) == 72);
static_assert(offsetof(Header, tapped_track_key) == 80);
static_assert(offsetof(Header, seq) == 88);
static_assert(sizeof(Frame) == 256);
static_assert(offsetof(Frame, master) == 40);
static_assert(offsetof(Frame, monitor) == 56);
static_assert(offsetof(Frame, master_volume_db) == 72);
static_assert(offsetof(Frame, track_count) == 76);
static_assert(offsetof(Frame, selected_track_key) == 80);
static_assert(offsetof(Frame, project_name) == 88);
static_assert(sizeof(TrackEntry) == 128);
static_assert(offsetof(TrackEntry, color) == 12);
static_assert(offsetof(TrackEntry, levels) == 24);
static_assert(offsetof(TrackEntry, name) == 40);
static_assert(kMasterRingOffset == 65920);
static_assert(kTrackRingOffset == 590272);

// Milliseconds since the Unix epoch (the heartbeat clock).
uint64_t nowMs();

// A NUL-padded UTF-8 name as a string.
std::string nameOf(const char* bytes, size_t size);

class Link {
public:
    Link() = default;
    ~Link() { close(); }
    Link(const Link&) = delete;
    Link& operator=(const Link&) = delete;

    // Maps the region; false (see lastError) when Studio is not publishing
    // or the region is not a version 1 link.
    bool open(const std::string& name = kRegionName);
    void close();
    bool isOpen() const { return base_ != nullptr; }
    const std::string& lastError() const { return error_; }

    // Tells Studio a visualizer is reading. Call every frame or so; Studio
    // stops publishing after kReaderTimeoutMs without one.
    void stampHeartbeat();
    uint64_t writerHeartbeatMs() const;
    bool writerAlive(uint64_t now) const;
    uint32_t writerPid() const;
    uint32_t sampleRate() const;

    // A consistent copy of the frame and the track table; false when every
    // try raced a write (rare: try again next frame).
    bool snapshot(Frame& frame, std::vector<TrackEntry>& tracks) const;

    // Asks Studio for one track's post-fader audio on the track ring (0: none).
    void requestTrack(uint64_t key);
    uint64_t requestedTrackKey() const;
    // The track the track ring carries now (0: none).
    uint64_t tappedTrackKey() const;

    // Audio since the previous call, planar stereo. `skipped` reports frames
    // lost because the reader fell more than a ring behind.
    size_t readMaster(float* left, float* right, size_t maxFrames, uint64_t& skipped);
    size_t readTrack(float* left, float* right, size_t maxFrames, uint64_t& skipped);

private:
    size_t readRing(size_t offset, uint64_t& cursor, float* left, float* right, size_t maxFrames,
                    uint64_t& skipped);
    Header* header() const { return reinterpret_cast<Header*>(base_); }

    uint8_t* base_ = nullptr;
    void* handle_ = nullptr; // Windows mapping handle
    uint64_t masterCursor_ = 0, trackCursor_ = 0;
    std::string error_;
};

} // namespace fblink
