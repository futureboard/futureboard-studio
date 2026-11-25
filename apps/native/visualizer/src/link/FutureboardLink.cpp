#include "FutureboardLink.hpp"

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstring>

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#else
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#endif

namespace fblink {

namespace {

template <typename T>
std::atomic_ref<T> atom(T& value) {
    return std::atomic_ref<T>(value);
}

// Overwrite margin kept from the write head when a late reader resyncs, as
// the Rust reader does.
constexpr uint64_t kMargin = 2048;

} // namespace

uint64_t nowMs() {
    using namespace std::chrono;
    return static_cast<uint64_t>(duration_cast<milliseconds>(system_clock::now().time_since_epoch()).count());
}

std::string nameOf(const char* bytes, size_t size) {
    size_t n = 0;
    while (n < size && bytes[n] != 0) ++n;
    return std::string(bytes, n);
}

bool Link::open(const std::string& name) {
    close();
#ifdef _WIN32
    const int wideLen = MultiByteToWideChar(CP_UTF8, 0, name.c_str(), -1, nullptr, 0);
    std::wstring wide(static_cast<size_t>(std::max(wideLen, 1)), L'\0');
    MultiByteToWideChar(CP_UTF8, 0, name.c_str(), -1, wide.data(), wideLen);
    HANDLE mapping = OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, wide.c_str());
    if (!mapping) {
        error_ = "Futureboard Studio is not running";
        return false;
    }
    void* view = MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, kTotalSize);
    if (!view) {
        CloseHandle(mapping);
        error_ = "cannot map the visualizer link";
        return false;
    }
    handle_ = mapping;
    base_ = static_cast<uint8_t*>(view);
#else
    const int fd = shm_open(name.c_str(), O_RDWR, 0);
    if (fd < 0) {
        error_ = "Futureboard Studio is not running";
        return false;
    }
    struct stat st {};
    if (fstat(fd, &st) != 0 || static_cast<size_t>(st.st_size) < kTotalSize) {
        ::close(fd);
        error_ = "the visualizer link has another size";
        return false;
    }
    void* view = mmap(nullptr, kTotalSize, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ::close(fd);
    if (view == MAP_FAILED) {
        error_ = "cannot map the visualizer link";
        return false;
    }
    base_ = static_cast<uint8_t*>(view);
#endif
    const Header* h = header();
    if (h->magic != kMagic || h->version != kVersion || h->total_size != kTotalSize ||
        h->master_ring_offset != kMasterRingOffset || h->track_ring_offset != kTrackRingOffset) {
        close();
        error_ = "Studio publishes another visualizer link version";
        return false;
    }
    masterCursor_ = atom(*reinterpret_cast<uint64_t*>(base_ + kMasterRingOffset)).load(std::memory_order_acquire);
    trackCursor_ = atom(*reinterpret_cast<uint64_t*>(base_ + kTrackRingOffset)).load(std::memory_order_acquire);
    error_.clear();
    return true;
}

void Link::close() {
    if (!base_) return;
#ifdef _WIN32
    UnmapViewOfFile(base_);
    CloseHandle(static_cast<HANDLE>(handle_));
    handle_ = nullptr;
#else
    munmap(base_, kTotalSize);
#endif
    base_ = nullptr;
}

void Link::stampHeartbeat() {
    if (base_) atom(header()->reader_heartbeat_ms).store(nowMs(), std::memory_order_release);
}

uint64_t Link::writerHeartbeatMs() const {
    return base_ ? atom(header()->writer_heartbeat_ms).load(std::memory_order_acquire) : 0;
}

bool Link::writerAlive(uint64_t now) const {
    const uint64_t seen = writerHeartbeatMs();
    return seen != 0 && (now < seen || now - seen < kWriterTimeoutMs);
}

uint32_t Link::writerPid() const { return base_ ? header()->writer_pid : 0; }

uint32_t Link::sampleRate() const {
    return base_ ? atom(header()->sample_rate).load(std::memory_order_acquire) : 0;
}

bool Link::snapshot(Frame& frame, std::vector<TrackEntry>& tracks) const {
    if (!base_) return false;
    auto seq = atom(header()->seq);
    for (int attempt = 0; attempt < 64; ++attempt) {
        const uint32_t before = seq.load(std::memory_order_acquire);
        if (before & 1u) continue;
        std::memcpy(&frame, base_ + kFrameOffset, sizeof frame);
        const size_t count = std::min<size_t>(frame.track_count, kMaxTracks);
        tracks.resize(count);
        if (count) std::memcpy(tracks.data(), base_ + kTracksOffset, count * sizeof(TrackEntry));
        std::atomic_thread_fence(std::memory_order_acquire);
        if (seq.load(std::memory_order_relaxed) == before) return true;
    }
    return false;
}

void Link::requestTrack(uint64_t key) {
    if (base_) atom(header()->requested_track_key).store(key, std::memory_order_release);
}

uint64_t Link::requestedTrackKey() const {
    return base_ ? atom(header()->requested_track_key).load(std::memory_order_acquire) : 0;
}

uint64_t Link::tappedTrackKey() const {
    return base_ ? atom(header()->tapped_track_key).load(std::memory_order_acquire) : 0;
}

size_t Link::readRing(size_t offset, uint64_t& cursor, float* left, float* right, size_t maxFrames,
                      uint64_t& skipped) {
    skipped = 0;
    if (!base_) return 0;
    uint8_t* ring = base_ + offset;
    const uint64_t head = atom(*reinterpret_cast<uint64_t*>(ring)).load(std::memory_order_acquire);
    const uint64_t window = kAudioFrames - kMargin;
    const uint64_t oldest = head > window ? head - window : 0;
    uint64_t start;
    if (cursor < oldest) {
        start = oldest;
        skipped = oldest - cursor;
    } else {
        start = std::min(cursor, head);
    }
    const size_t frames = static_cast<size_t>(std::min<uint64_t>(head - start, maxFrames));
    const float* l = reinterpret_cast<const float*>(ring + kRingHeadSize);
    const float* r = l + kAudioFrames;
    for (size_t i = 0; i < frames; ++i) {
        const size_t slot = static_cast<size_t>(start + i) & (kAudioFrames - 1);
        left[i] = l[slot];
        right[i] = r[slot];
    }
    cursor = start + frames;
    return frames;
}

size_t Link::readMaster(float* left, float* right, size_t maxFrames, uint64_t& skipped) {
    return readRing(kMasterRingOffset, masterCursor_, left, right, maxFrames, skipped);
}

size_t Link::readTrack(float* left, float* right, size_t maxFrames, uint64_t& skipped) {
    return readRing(kTrackRingOffset, trackCursor_, left, right, maxFrames, skipped);
}

} // namespace fblink
