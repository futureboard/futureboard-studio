#include "MeterViews.hpp"

#include <algorithm>
#include <cmath>
#include <cstdio>

namespace fbvis {

namespace {

using lcd::Canvas;
using mixviz::Rect;

constexpr uint8_t kDim = 60;
constexpr uint8_t kMid = 150;
constexpr float kFloorDb = -60.0f;
constexpr float kTopDb = 6.0f;

void dottedH(Canvas& c, int x, int y, int w, uint8_t v = kDim, int step = 2) {
    for (int i = 0; i < w; i += step) c.pixel(x + i, y, v);
}

void dottedV(Canvas& c, int x, int y, int h, uint8_t v = kDim, int step = 2) {
    for (int i = 0; i < h; i += step) c.pixel(x, y + i, v);
}

// dB -> pixels above the bottom of a `span`-pixel scale.
int dbPixels(float db, int span) {
    const float t = std::clamp((db - kFloorDb) / (kTopDb - kFloorDb), 0.0f, 1.0f);
    return static_cast<int>(std::lround(t * span));
}

std::string fmtDb(float db, bool sign = false) {
    if (db <= kFloorDb - 0.05f) return "-INF";
    char b[16];
    std::snprintf(b, sizeof b, sign ? "%+.1f" : "%.1f", db);
    return b;
}

// One vertical channel bar: RMS solid, peak above it dimmer, a hold line,
// dimmed overall for a muted track.
void channelBar(Canvas& c, int x, int yTop, int w, int h, const mixviz::PeakMeter& pk,
                const mixviz::PeakMeter& rms, int ch, bool muted) {
    const int bottom = yTop + h - 1;
    const int peakPx = dbPixels(pk.db[ch], h);
    const int rmsPx = std::min(peakPx, dbPixels(rms.db[ch], h));
    const uint8_t bright = muted ? 90 : 255;
    const uint8_t soft = muted ? 50 : 120;
    for (int i = 0; i < peakPx; ++i) {
        // Segmented like an LED bar: every third row stays dark.
        if (i % 3 == 2) continue;
        c.hline(x, bottom - i, w, i < rmsPx ? bright : soft);
    }
    if (pk.hold[ch] > kFloorDb) {
        const int hy = bottom - std::min(h - 1, dbPixels(pk.hold[ch], h));
        c.hline(x, hy, w, bright);
    }
}

// The dB scale's tick rows, shared by the bridge and the mixer.
constexpr float kTicks[] = {6.0f, 0.0f, -6.0f, -12.0f, -24.0f, -36.0f, -48.0f};

void scale(Canvas& c, int x, int yTop, int h, bool labels) {
    for (float db : kTicks) {
        const int y = yTop + h - 1 - dbPixels(db, h);
        if (labels) {
            char b[8];
            std::snprintf(b, sizeof b, "%d", static_cast<int>(db));
            c.text(x + 19 - Canvas::textWidth(b), y - 3, b, 1, db == 0.0f ? 255 : kMid);
        } else {
            c.hline(x, y, 2, db == 0.0f ? 200 : kDim);
        }
    }
}

void clipLamp(Canvas& c, int x, int y, int w, const MeterState& m) {
    if (m.clipAge < 2.0f || m.peak.hold[0] >= -0.1f || m.peak.hold[1] >= -0.1f) c.fillRect(x, y, w, 2);
    else c.hline(x, y + 1, w, kDim);
}

const MeterState& meterOf(const Session& s, uint64_t key) {
    static const MeterState kEmpty{};
    const auto it = s.meters.find(key);
    return it == s.meters.end() ? kEmpty : it->second;
}

PageInfo paginate(int count, int perPage, int& page) {
    PageInfo info;
    info.perPage = std::max(1, perPage);
    info.pages = std::max(1, (count + info.perPage - 1) / info.perPage);
    page = std::clamp(page, 0, info.pages - 1);
    info.page = page;
    return info;
}

std::string trackLabel(const Session& s, uint64_t key, size_t maxChars) {
    const int index = s.indexOf(key);
    if (index < 0) return {};
    const auto& t = s.tracks[static_cast<size_t>(index)];
    char b[16];
    std::snprintf(b, sizeof b, "%d ", index + 1);
    return lcdText(b + fblink::nameOf(t.name, sizeof t.name), maxChars);
}

} // namespace

std::string lcdText(const std::string& utf8, size_t maxChars) {
    std::string out;
    for (size_t i = 0; i < utf8.size() && out.size() < maxChars; ++i) {
        const unsigned char ch = static_cast<unsigned char>(utf8[i]);
        if (ch < 0x80) {
            out += (ch >= 0x20 && ch < 0x7F) ? static_cast<char>(ch) : '?';
        } else if ((ch & 0xC0) != 0x80) {
            out += '?'; // one mark per non-ASCII character, not per byte
        }
    }
    return out;
}

void Session::updateMeters(float dt) {
    for (auto& [key, m] : meters) m.seen = false;
    for (const auto& t : tracks) {
        MeterState& m = meters[t.key];
        m.seen = true;
        m.peak.update({t.levels.peak_l, t.levels.peak_r}, dt);
        m.rms.update({t.levels.rms_l, t.levels.rms_r}, dt);
        m.clipAge = (t.flags & fblink::flags::Clipped) ? 0.0f : m.clipAge + dt;
    }
    std::erase_if(meters, [](const auto& entry) { return !entry.second.seen; });
    master.peak.update({frame.master.peak_l, frame.master.peak_r}, dt);
    master.rms.update({frame.master.rms_l, frame.master.rms_r}, dt);
    const bool masterClip = frame.master.peak_l >= 1.0f || frame.master.peak_r >= 1.0f;
    master.clipAge = masterClip ? 0.0f : master.clipAge + dt;
    monitor.peak.update({frame.monitor.peak_l, frame.monitor.peak_r}, dt);
    monitor.rms.update({frame.monitor.peak_l, frame.monitor.peak_r}, dt);
}

const fblink::TrackEntry* Session::find(uint64_t key) const {
    const int index = indexOf(key);
    return index < 0 ? nullptr : &tracks[static_cast<size_t>(index)];
}

int Session::indexOf(uint64_t key) const {
    if (!key) return -1;
    for (size_t i = 0; i < tracks.size(); ++i)
        if (tracks[i].key == key) return static_cast<int>(i);
    return -1;
}

// ------------------------------------------------------------------ Meters

void drawMeters(Canvas& c, Rect r, const Session& s, bool list, int& page, std::vector<HitColumn>& hits,
                PageInfo& info) {
    hits.clear();
    const int count = static_cast<int>(s.tracks.size());

    if (list) {
        // Named rows: "NN NAME" then a stereo bar, several columns across.
        constexpr int rowH = 9, colW = 128;
        const int rows = std::max(1, (r.h - 10) / rowH);
        const int cols = std::max(1, r.w / colW);
        info = paginate(count, rows * cols, page);
        const int first = info.page * info.perPage;
        for (int i = 0; i < info.perPage && first + i < count; ++i) {
            const auto& t = s.tracks[static_cast<size_t>(first + i)];
            const MeterState& m = meterOf(s, t.key);
            const int x = r.x + (i / rows) * colW, y = r.y + (i % rows) * rowH;
            const bool focus = t.key == s.focusKey;
            const bool muted = (t.flags & fblink::flags::Muted) != 0;
            char num[8];
            std::snprintf(num, sizeof num, "%3d", first + i + 1);
            const std::string name = lcdText(fblink::nameOf(t.name, sizeof t.name), 8);
            if (focus) c.fillRect(x, y, 70, rowH - 1, 255);
            const uint8_t ink = focus ? 0 : (muted ? 90 : 255);
            c.text(x + 1, y + 1, num, 1, focus ? 0 : kMid);
            c.text(x + 22 + 2 * t.depth, y + 1, name, 1, ink);
            const int bx = x + 72, bw = colW - 76;
            for (int ch = 0; ch < 2; ++ch) {
                const int len = dbPixels(m.peak.db[ch], bw);
                const int rmsLen = std::min(len, dbPixels(m.rms.db[ch], bw));
                for (int k = 0; k < len; k += 2)
                    c.vline(bx + k, y + 1 + ch * 4, 3, k < rmsLen ? (muted ? 90 : 255) : (muted ? 50 : 120));
                if (m.peak.hold[ch] > kFloorDb) c.vline(bx + std::min(bw - 1, dbPixels(m.peak.hold[ch], bw)), y + 1 + ch * 4, 3);
            }
            c.vline(bx + dbPixels(0.0f, bw), y, rowH - 1, kDim);
            hits.push_back({x, x + colW - 2, y, y + rowH - 1, t.key});
        }
    } else {
        // Bridge: a scale at the left, one 2x2-bar column per track, master
        // at the right.
        constexpr int colW = 6, scaleW = 20, masterW = 16;
        const int meterTop = r.y + 4, meterH = r.h - 4 - 14;
        const int avail = r.w - scaleW - masterW - 4;
        info = paginate(count, avail / colW, page);
        scale(c, r.x, meterTop, meterH, true);
        const int x0 = r.x + scaleW + 2;
        for (float db : kTicks) {
            const int y = meterTop + meterH - 1 - dbPixels(db, meterH);
            dottedH(c, x0, y, avail, db == 0.0f ? 90 : 35, db == 0.0f ? 2 : 4);
        }
        const int first = info.page * info.perPage;
        for (int i = 0; i < info.perPage && first + i < count; ++i) {
            const auto& t = s.tracks[static_cast<size_t>(first + i)];
            const MeterState& m = meterOf(s, t.key);
            const int x = x0 + i * colW;
            const bool muted = (t.flags & fblink::flags::Muted) != 0;
            clipLamp(c, x, meterTop - 4, colW - 1, m);
            channelBar(c, x, meterTop, 2, meterH, m.peak, m.rms, 0, muted);
            channelBar(c, x + 2, meterTop, 2, meterH, m.peak, m.rms, 1, muted);
            const int markY = meterTop + meterH + 1;
            if (t.key == s.focusKey) c.fillRect(x, markY, colW - 1, 3);
            else if (t.key == s.hoverKey) c.hline(x, markY + 2, colW - 1);
            else if (t.flags & fblink::flags::Armed) c.pixel(x + 2, markY + 1);
            if (t.kind == fblink::kind::Group || t.kind == fblink::kind::Bus || t.kind == fblink::kind::Return)
                c.pixel(x + 2, meterTop - 6, kMid);
            if ((first + i + 1) % 10 == 0) dottedV(c, x + colW - 1, meterTop, meterH, 35, 3);
            hits.push_back({x, x + colW - 1, r.y, meterTop + meterH + 4, t.key});
        }
        // Master
        const int mx = r.x + r.w - masterW;
        c.vline(mx - 3, r.y, r.h - 12, kDim);
        clipLamp(c, mx, meterTop - 4, masterW - 2, s.master);
        channelBar(c, mx, meterTop, 6, meterH, s.master.peak, s.master.rms, 0, false);
        channelBar(c, mx + 7, meterTop, 6, meterH, s.master.peak, s.master.rms, 1, false);
        c.text(r.x + r.w - 18, meterTop + meterH + 2, "MST", 1, kMid);
    }

    // Footer: the hovered (or focused) track and its numbers.
    const uint64_t key = s.hoverKey ? s.hoverKey : s.focusKey;
    const int fy = r.y + r.h - 8;
    if (const auto* t = s.find(key)) {
        const MeterState& m = meterOf(s, key);
        const float peak = std::max(m.peak.hold[0], m.peak.hold[1]);
        std::string line = trackLabel(s, key, 20) + "  PK " + fmtDb(peak) + "  FDR " + fmtDb(t->volume_db, true);
        if (t->flags & fblink::flags::Muted) line += " M";
        if (t->flags & fblink::flags::Solo) line += " S";
        if (t->flags & fblink::flags::Armed) line += " R";
        c.text(r.x + 2, fy, line, 1, list ? kMid : 255);
    } else {
        char b[48];
        std::snprintf(b, sizeof b, count ? "%d TRACKS  CLICK OR UP/DN TO WATCH ONE" : "NO TRACKS", count);
        c.text(r.x + 2, fy, b, 1, kMid);
    }
}

// ------------------------------------------------------------------- Mixer

namespace {

void strip(Canvas& c, int x, int y, int w, int h, const std::string& name, const MeterState& m, float faderDb,
           float pan, uint8_t trackFlags, bool focus, bool hover, bool showFlags, bool showPan) {
    const bool muted = (trackFlags & fblink::flags::Muted) != 0;
    // Name, inverted for the focused track.
    if (focus) c.fillRect(x, y, w - 1, 8, 255);
    c.textCentered(x + (w - 1) / 2, y, lcdText(name, static_cast<size_t>((w - 1) / 6)), 1, focus ? 0 : 255);
    if (hover && !focus) c.hline(x, y + 8, w - 1, kMid);

    // M / S / R lamps.
    const int ly = y + 10;
    if (showFlags) {
        const char* letters[] = {"M", "S", "R"};
        const uint8_t bits[] = {fblink::flags::Muted, fblink::flags::Solo, fblink::flags::Armed};
        for (int i = 0; i < 3; ++i) {
            const int lx = x + 1 + i * 9;
            if (trackFlags & bits[i]) {
                c.fillRect(lx, ly, 8, 9, 255);
                c.text(lx + 2, ly + 1, letters[i], 1, 0);
            } else {
                c.rect(lx, ly, 8, 9, kDim);
                c.text(lx + 2, ly + 1, letters[i], 1, kDim);
            }
        }
    }

    // Meter (two bars) and fader on one dB scale.
    const int mTop = y + 23, mH = h - 23 - 26;
    clipLamp(c, x + 1, mTop - 3, 9, m);
    channelBar(c, x + 1, mTop, 4, mH, m.peak, m.rms, 0, muted);
    channelBar(c, x + 6, mTop, 4, mH, m.peak, m.rms, 1, muted);
    const int fx = x + 19;
    scale(c, fx - 6, mTop, mH, false);
    c.vline(fx, mTop, mH, kDim);
    const int capY = mTop + mH - 1 - dbPixels(faderDb, mH);
    c.fillRect(fx - 4, capY - 1, 9, 3, 255);
    c.hline(fx - 3, capY, 7, 0);

    // Fader dB, peak hold, pan.
    const int ty = mTop + mH + 2;
    c.textCentered(x + (w - 1) / 2, ty, fmtDb(faderDb), 1, 255);
    const float hold = std::max(m.peak.hold[0], m.peak.hold[1]);
    c.textCentered(x + (w - 1) / 2, ty + 8, fmtDb(hold), 1, hold >= -0.1f ? 255 : kMid);
    if (showPan) {
        const int px = x + 2, pw = w - 5, py = ty + 17;
        c.hline(px, py + 2, pw, kDim);
        c.vline(px + pw / 2, py, 5, kMid);
        const int dot = px + pw / 2 + static_cast<int>(std::lround(std::clamp(pan, -1.0f, 1.0f) * (pw / 2)));
        c.fillRect(dot - 1, py, 3, 5, 255);
    }
}

} // namespace

void drawMixer(Canvas& c, Rect r, const Session& s, int& page, std::vector<HitColumn>& hits, PageInfo& info) {
    hits.clear();
    constexpr int stripW = 34;
    const int count = static_cast<int>(s.tracks.size());
    const int avail = r.w - stripW - 3;
    info = paginate(count, avail / stripW, page);
    const int first = info.page * info.perPage;
    for (int i = 0; i < info.perPage && first + i < count; ++i) {
        const auto& t = s.tracks[static_cast<size_t>(first + i)];
        const int x = r.x + i * stripW;
        // A multi-output instrument's channels share its name ("Kontakt 8
        // Out Ch 3"): a five-character strip shows which output it is.
        std::string name = fblink::nameOf(t.name, sizeof t.name);
        if (const size_t out = name.rfind(" Out "); t.kind == fblink::kind::PluginOutput && out != std::string::npos)
            name = name.substr(out + 5);
        strip(c, x, r.y, stripW, r.h, name, meterOf(s, t.key), t.volume_db, t.pan,
              t.flags, t.key == s.focusKey, t.key == s.hoverKey, true, true);
        dottedV(c, x + stripW - 1, r.y, r.h, kDim, 2);
        hits.push_back({x, x + stripW - 2, r.y, r.y + r.h, t.key});
    }
    if (count == 0) c.textCentered(r.x + avail / 2, r.y + r.h / 2 - 4, "NO TRACKS", 1, kMid);
    const int mx = r.x + r.w - stripW;
    c.vline(mx - 2, r.y, r.h, 120);
    strip(c, mx, r.y, stripW, r.h, "MST", s.master, s.frame.master_volume_db, 0.0f, 0, false, false, false, false);
}

// --------------------------------------------------------------- Transport

void drawTransport(Canvas& c, Rect r, const Session& s, int sampleRate) {
    const auto& f = s.frame;
    const int X = r.x + 2, Y = r.y;
    char b[64];

    // Bar.beat.tick from quarter-note beats and the signature at the playhead.
    const double quarterPerBeat = f.time_sig_den ? 4.0 / f.time_sig_den : 1.0;
    const double beatsPerBar = std::max(1u, f.time_sig_num) * quarterPerBeat;
    const double pos = std::max(0.0, f.position_beats);
    const long long bar = static_cast<long long>(pos / beatsPerBar) + 1;
    const double inBar = std::fmod(pos, beatsPerBar);
    const int beat = static_cast<int>(inBar / quarterPerBeat) + 1;
    const int tick = static_cast<int>(std::fmod(inBar, quarterPerBeat) / quarterPerBeat * 100.0);
    std::snprintf(b, sizeof b, "%4lld.%d.%02d", bar, beat, tick);
    c.text(X, Y + 3, b, 3);

    const double secs = std::max(0.0, f.position_seconds);
    const int mins = static_cast<int>(secs / 60.0);
    std::snprintf(b, sizeof b, "%02d:%06.3f", mins, secs - mins * 60.0);
    c.text(X, Y + 29, b, 2);

    std::snprintf(b, sizeof b, "%.2f BPM", static_cast<double>(f.bpm));
    c.text(X + 132, Y + 29, b, 1);
    std::snprintf(b, sizeof b, "%u/%u", f.time_sig_num, f.time_sig_den);
    c.text(X + 132, Y + 38, b, 1, kMid);

    // Transport lamps.
    const int ly = Y + 50;
    const bool playing = f.transport & fblink::transport::Playing;
    const bool recording = f.transport & fblink::transport::Recording;
    const bool looping = f.transport & fblink::transport::Looping;
    for (int i = 0; i < 7; ++i) c.vline(X + i, ly + i / 2, 7 - (i / 2) * 2, playing ? 255 : kDim); // play triangle
    c.text(X + 10, ly, playing ? "PLAY" : "STOP", 1, playing ? 255 : kMid);
    if (recording) c.fillCircle(X + 48, ly + 3, 3);
    else c.circle(X + 48, ly + 3, 3, kDim);
    c.text(X + 55, ly, "REC", 1, recording ? 255 : kDim);
    c.text(X + 80, ly, "LOOP", 1, looping ? 255 : kDim);
    if (looping) c.hline(X + 80, ly + 8, 23);

    // Master and Control Room levels.
    auto levelRow = [&](int y, const char* label, const MeterState& m) {
        c.text(X, y, label, 1, kMid);
        const int bx = X + 20, bw = r.w - 20 - 48;
        for (int ch = 0; ch < 2; ++ch) {
            const int by = y + ch * 5;
            c.rect(bx, by, bw, 4, kDim);
            const int len = dbPixels(m.peak.db[ch], bw - 2);
            for (int k = 0; k < len; k += 2) c.vline(bx + 1 + k, by + 1, 2);
            if (m.peak.hold[ch] > kFloorDb) c.vline(bx + 1 + std::min(bw - 3, dbPixels(m.peak.hold[ch], bw - 2)), by, 4);
        }
        const float hold = std::max(m.peak.hold[0], m.peak.hold[1]);
        c.text(r.x + r.w - 44, y + 1, fmtDb(hold), 1, hold >= -0.1f ? 255 : kMid);
    };
    levelRow(Y + 63, "MST", s.master);
    levelRow(Y + 76, "MON", s.monitor);
    std::snprintf(b, sizeof b, "FADER %s dB", fmtDb(f.master_volume_db, true).c_str());
    c.text(X, Y + 89, b, 1, kMid);

    // Project and link.
    const std::string project = lcdText(fblink::nameOf(f.project_name, sizeof f.project_name), 40);
    c.text(X, Y + r.h - 18, project, 1, 255);
    std::snprintf(b, sizeof b, "%zu TRACKS  %.1f kHz", s.tracks.size(), sampleRate / 1000.0);
    c.text(X, Y + r.h - 9, b, 1, kMid);
}

} // namespace fbvis
