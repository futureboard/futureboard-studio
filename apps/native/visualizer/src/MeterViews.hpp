#pragma once
// The views that draw Studio's track table: every meter, mixer strips, and
// the transport. The audio views (loudness, spectrum, imager, scope) come
// from mixviz.

#include "Canvas.hpp"
#include "link/FutureboardLink.hpp"
#include "mixviz/Views.hpp"

#include <cstdint>
#include <string>
#include <unordered_map>
#include <vector>

namespace fbvis {

// Ballistics for one meter pair (dB with decay, held peaks, clip latch).
struct MeterState {
    mixviz::PeakMeter peak;
    mixviz::PeakMeter rms;
    float clipAge = 1e9f; // seconds since the last clipped frame
    bool seen = false;
};

// What the track views share: the latest table plus per-track ballistics.
struct Session {
    fblink::Frame frame{};
    std::vector<fblink::TrackEntry> tracks;
    std::unordered_map<uint64_t, MeterState> meters;
    MeterState master, monitor;
    uint64_t focusKey = 0; // the track whose audio the Track tab shows
    uint64_t hoverKey = 0; // the meter under the mouse
    bool connected = false;

    // Advances every meter's ballistics from the latest snapshot.
    void updateMeters(float dt);
    const fblink::TrackEntry* find(uint64_t key) const;
    int indexOf(uint64_t key) const;
};

// A clickable column: canvas x range -> track key, rebuilt every draw.
struct HitColumn {
    int x0, x1, y0, y1;
    uint64_t key;
};

struct PageInfo {
    int page = 0, pages = 1, perPage = 0;
};

// Tab 1: every track's meter side by side (bridge) or as named rows (list),
// master at the right. `page` is clamped and written back.
void drawMeters(lcd::Canvas& c, mixviz::Rect r, const Session& s, bool list, int& page,
                std::vector<HitColumn>& hits, PageInfo& info);

// Tab 2: mixer strips with fader, pan, mute/solo/arm and peak readouts.
void drawMixer(lcd::Canvas& c, mixviz::Rect r, const Session& s, int& page, std::vector<HitColumn>& hits,
               PageInfo& info);

// Transport, position, tempo, master and Control Room levels.
void drawTransport(lcd::Canvas& c, mixviz::Rect r, const Session& s, int sampleRate);

// Printable ASCII of a UTF-8 name (the 5x7 font has no other glyphs),
// upper-cased, cut to `maxChars`.
std::string lcdText(const std::string& utf8, size_t maxChars);

} // namespace fbvis
