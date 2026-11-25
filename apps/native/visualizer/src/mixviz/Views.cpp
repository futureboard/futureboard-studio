#include "Views.hpp"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <string>

namespace mixviz {

namespace {

constexpr uint8_t kDim = 60;    // grid / guides
constexpr uint8_t kMid = 150;   // secondary marks

using lcd::Canvas;

void dottedH(Canvas& c, int x, int y, int w, uint8_t v = kDim, int step = 2) {
    for (int i = 0; i < w; i += step) c.pixel(x + i, y, v);
}

void dottedV(Canvas& c, int x, int y, int h, uint8_t v = kDim, int step = 2) {
    for (int i = 0; i < h; i += step) c.pixel(x, y + i, v);
}

std::string fmtLufs(float v) {
    if (v <= -99.0f) return " -INF";
    char b[16];
    std::snprintf(b, sizeof b, "%5.1f", v);
    return b;
}

int mapRange(float v, float lo, float hi, int pixels) {
    const float t = std::clamp((v - lo) / (hi - lo), 0.0f, 1.0f);
    return static_cast<int>(std::lround(t * pixels));
}

// Horizontal bar with segmented fill, value mapped from [lo, hi].
void hbar(Canvas& c, int x, int y, int w, int h, float v, float lo, float hi) {
    c.rect(x, y, w, h);
    const int fill = mapRange(v, lo, hi, w - 4);
    for (int i = 0; i < fill; i += 2) c.vline(x + 2 + i, y + 2, h - 4);
}

// Vertical LED-style peak bar.
void vbar(Canvas& c, int x, int y, int w, int h, float db, float holdDb, float lo, float hi) {
    c.rect(x, y, w, h);
    const int inner = h - 4;
    const int fill = mapRange(db, lo, hi, inner);
    for (int i = 0; i < fill; i += 2) c.hline(x + 2, y + h - 3 - i, w - 4);
    if (holdDb > lo) {
        const int hy = y + h - 3 - mapRange(holdDb, lo, hi, inner - 1);
        c.hline(x + 2, hy, w - 4, 255);
    }
    // 0 dBFS clip marker
    if (holdDb >= -0.1f) c.fillRect(x, y - 3, w, 2);
}

// Centered bipolar bar, value in [-range, +range].
void cbar(Canvas& c, int x, int y, int w, int h, float v, float range) {
    c.rect(x, y, w, h);
    const int mid = x + w / 2;
    c.vline(mid, y - 2, h + 4);
    const int len = mapRange(std::fabs(v), 0.0f, range, w / 2 - 2);
    if (v >= 0) c.fillRect(mid + 1, y + 2, len, h - 4);
    else c.fillRect(mid - len, y + 2, len, h - 4);
}

} // namespace

// ---------------------------------------------------------------- PeakMeter

void PeakMeter::update(std::array<float, 2> linear, float dt) {
    for (int ch = 0; ch < 2; ++ch) {
        const float d = toDb(linear[ch]);
        db[ch] = std::max(d, db[ch] - 26.0f * dt);
        if (d >= hold[ch]) { hold[ch] = d; holdAge[ch] = 0.0f; }
        else if ((holdAge[ch] += dt) > 1.5f) hold[ch] -= 30.0f * dt;
    }
}

// ---------------------------------------------------------------- Imager

void Imager::draw(Canvas& c, int x, int y, int side, const float* l, const float* r, size_t n, float dt) {
    if (side != side_) {
        side_ = side;
        phosphor_.assign(static_cast<size_t>(side) * side, 0.0f);
    }
    const float decay = std::exp(-dt / 0.07f);
    for (float& p : phosphor_) p *= decay;

    float peak = 0.0f;
    for (size_t i = 0; i < n; ++i) {
        peak = std::max(peak, std::max(std::fabs(l[i] + r[i]), std::fabs(r[i] - l[i])) * 0.70710678f);
    }
    env_ = std::max(peak, env_ * std::exp(-dt / 1.5f));
    const float gain = autoGain ? std::min(0.92f / std::max(env_, 0.01f), 40.0f) : 0.70710678f;

    const float half = side * 0.5f - 1.0f;
    const float cx = side * 0.5f, cy = side * 0.5f;
    for (size_t i = 0; i < n; ++i) {
        const float m = (l[i] + r[i]) * 0.70710678f;
        const float s = (r[i] - l[i]) * 0.70710678f;
        const int px = static_cast<int>(cx + std::clamp(s * gain, -1.0f, 1.0f) * half);
        const int py = static_cast<int>(cy - std::clamp(m * gain, -1.0f, 1.0f) * half);
        if (px < 0 || py < 0 || px >= side || py >= side) continue;
        float& p = phosphor_[static_cast<size_t>(py) * side + px];
        p = std::min(1.0f, p + 0.3f);
    }

    // Guides: L and R diagonals, M vertical, S horizontal.
    const int s1 = side - 1;
    for (int i = 0; i < side; i += 3) {
        c.pixel(x + i, y + i, kDim);
        c.pixel(x + s1 - i, y + i, kDim);
        c.pixel(x + side / 2, y + i, kDim);
        c.pixel(x + i, y + side / 2, kDim);
    }
    for (int j = 0; j < side; ++j)
        for (int i = 0; i < side; ++i) {
            const float p = phosphor_[static_cast<size_t>(j) * side + i];
            if (p < 0.02f) continue;
            const auto v = static_cast<uint8_t>(std::min(255.0f, 70.0f + 185.0f * std::sqrt(p)));
            c.pixel(x + i, y + j, std::max(v, c.get(x + i, y + j)));
        }
}

// ---------------------------------------------------------------- Loudness

void drawLoudness(Canvas& c, Rect r, const FrameData& f, bool compact) {
    const LoudnessMeter& L = *f.loudness;
    const PeakMeter& pk = *f.peaks;
    const int X = r.x, Y = r.y;
    char b[32];

    if (compact) {
        c.text(X + 2, Y + 12, fmtLufs(L.shortTerm()), 2);
        c.text(X + 2, Y + 29, "LUFS S", 1, kMid);
        c.text(X + 72, Y + 11, "M" + fmtLufs(L.momentary()));
        c.text(X + 72, Y + 21, "I" + fmtLufs(L.integrated()));
        std::snprintf(b, sizeof b, "R%5.1f", L.range());
        c.text(X + 72, Y + 31, b);
        hbar(c, X + 2, Y + 41, r.w - 4, 7, L.momentary(), -48.0f, 0.0f);
        hbar(c, X + 2, Y + 50, r.w - 4, 5, pk.db[0], -60.0f, 0.0f);
        hbar(c, X + 2, Y + 56, r.w - 4, 5, pk.db[1], -60.0f, 0.0f);
        return;
    }

    // Left column: readouts
    c.text(X + 2, Y + 3, "SHORT-TERM", 1, kMid);
    c.text(X + 2, Y + 13, fmtLufs(L.shortTerm()), 3);
    c.text(X + 2, Y + 37, "LUFS", 1, kMid);
    c.text(X + 2, Y + 49, "MOM " + fmtLufs(L.momentary()));
    c.text(X + 2, Y + 59, "INT " + fmtLufs(L.integrated()));
    std::snprintf(b, sizeof b, "LRA %5.1f", L.range());
    c.text(X + 2, Y + 69, b);
    c.text(X + 2, Y + 79, "PK  " + fmtLufs(L.maxPeakDb()));
    if (L.integrated() > -99.0f && L.maxPeakDb() > -99.0f) {
        std::snprintf(b, sizeof b, "PLR %5.1f", L.maxPeakDb() - L.integrated());
        c.text(X + 2, Y + 89, b, 1, kMid);
    }

    // Short-term history graph, -42..-6 LUFS
    const int gx = X + 94, gy = Y + 2, gw = r.w - 94 - 22, gh = 88;
    const float top = -6.0f, bot = -42.0f;
    c.rect(gx, gy, gw, gh);
    auto yOf = [&](float v) { return gy + gh - 2 - mapRange(v, bot, top, gh - 4); };
    for (int lv = -12; lv >= -36; lv -= 6) dottedH(c, gx + 1, yOf(static_cast<float>(lv)), gw - 2);
    for (int i = 0; i < gw - 2; i += 4) c.hline(gx + 1 + i, yOf(-14.0f), 2, kMid); // streaming target
    c.text(gx + gw - 20, yOf(-14.0f) - 9, "-14", 1, kMid);
    c.text(gx + 2, gy + 2, "-6", 1, kDim);
    c.text(gx + 2, gy + gh - 9, "-42", 1, kDim);

    const auto& hist = L.shortTermHistory();
    const int pts = std::min<int>(static_cast<int>(hist.size()), gw - 2);
    int prevY = -1;
    for (int i = 0; i < pts; ++i) {
        const float v = hist[hist.size() - pts + i];
        const int x = gx + gw - 1 - pts + i;
        if (v <= -99.0f) { prevY = -1; continue; }
        const int y = yOf(v);
        for (int yy = y + 1; yy < gy + gh - 1; yy += 2) c.pixel(x, yy, 45); // area fill
        if (prevY >= 0) c.vline(x, std::min(prevY, y), std::abs(prevY - y) + 1);
        else c.pixel(x, y);
        prevY = y;
    }

    // L/R sample peak bars
    const int mx = X + r.w - 19;
    vbar(c, mx, gy + 3, 8, gh - 3, pk.db[0], pk.hold[0], -60.0f, 0.0f);
    vbar(c, mx + 10, gy + 3, 8, gh - 3, pk.db[1], pk.hold[1], -60.0f, 0.0f);
    c.text(mx + 2, Y + 92, "L", 1, kMid);
    c.text(mx + 12, Y + 92, "R", 1, kMid);

    // Momentary bar with scale, -48..0
    const int bx = X + 10, bw = r.w - 12, by = Y + 101;
    c.text(X + 2, by, "M", 1, kMid);
    hbar(c, bx, by, bw, 7, L.momentary(), -48.0f, 0.0f);
    for (int lv = -48; lv <= 0; lv += 12) {
        const int tx = bx + 2 + mapRange(static_cast<float>(lv), -48.0f, 0.0f, bw - 5);
        const std::string s = std::to_string(lv);
        const int tw = Canvas::textWidth(s);
        c.text(std::clamp(tx - tw / 2, bx, bx + bw - tw), by + 10, s, 1, kMid);
    }
}

// ---------------------------------------------------------------- Spectrum

void drawSpectrum(Canvas& c, Rect r, const FrameData& f, bool compact) {
    const SpectrumAnalyzer& S = *f.spectrum;
    const int gx = r.x, gy = r.y + (compact ? 9 : 1), gw = r.w, gh = r.h - (compact ? 9 : 11);
    const float top = 0.0f, bot = -78.0f;
    auto yOf = [&](float db) { return gy + gh - 1 - mapRange(db, bot, top, gh - 1); };
    auto xOf = [&](float hz) {
        return gx + static_cast<int>(std::lround(std::log(hz / 20.0f) / std::log(1000.0f) * (gw - 1)));
    };

    for (int db = -12; db > bot; db -= 12) dottedH(c, gx, yOf(static_cast<float>(db)), gw, kDim, compact ? 3 : 2);
    struct Mark { float hz; const char* label; bool major; };
    static constexpr Mark marks[] = {{50, "50", false},  {100, "100", true}, {200, "200", false},
                                     {500, "500", false}, {1000, "1k", true}, {2000, "2k", false},
                                     {5000, "5k", false}, {10000, "10k", true}};
    for (const auto& m : marks) {
        if (compact && !m.major) continue;
        const int x = xOf(m.hz);
        dottedV(c, x, gy, gh, m.major ? 90 : kDim, m.major ? 2 : 3);
        if (!compact) c.textCentered(x, r.y + r.h - 8, m.label, 1, kMid);
    }
    if (!compact) {
        c.text(gx + 1, gy + 1, "0dB", 1, kDim);
        c.text(gx + 1, yOf(-72.0f) - 8, "-72", 1, kDim);
    }

    const int bottom = gy + gh - 1;
    int prevY = -1;
    for (int x = 0; x < gw; ++x) {
        const int b0 = x * SpectrumAnalyzer::kBands / gw;
        const int b1 = std::max(b0 + 1, (x + 1) * SpectrumAnalyzer::kBands / gw);
        float db = -200.0f, pk = -200.0f;
        for (int b = b0; b < b1; ++b) {
            db = std::max(db, S.band(b));
            pk = std::max(pk, S.peak(b));
        }
        const int y = yOf(db);
        const int px = gx + x;
        for (int yy = y + 1; yy <= bottom; ++yy) c.pixel(px, yy, ((yy ^ px) & 1) ? 90 : 55);
        if (prevY >= 0) c.vline(px, std::min(prevY, y), std::abs(prevY - y) + 1);
        else c.pixel(px, y);
        prevY = y;
        if (pk > bot && yOf(pk) < y) c.pixel(px, yOf(pk), 200);
    }
}

// ---------------------------------------------------------------- Imager

void drawImager(Canvas& c, Rect r, Imager& imager, const FrameData& f, bool compact) {
    const StereoStats& st = *f.stereo;
    char b[32];

    if (compact) {
        const int side = r.h - 1;
        const int sx = r.x + r.w - side - 1;
        imager.draw(c, sx, r.y, side, f.newL, f.newR, f.newCount, f.dt);
        const int X = r.x + 2, Y = r.y;
        std::snprintf(b, sizeof b, "%+.2f", st.correlation);
        c.text(X, Y + 12, "CORR", 1, kMid);
        c.text(X, Y + 22, b);
        cbar(c, X, Y + 32, 56, 6, st.correlation, 1.0f);
        std::snprintf(b, sizeof b, "W %3d%%", static_cast<int>(std::lround(st.width * 100)));
        c.text(X, Y + 43, b);
        if (std::fabs(st.balanceDb) < 0.1f) std::snprintf(b, sizeof b, "B   C");
        else std::snprintf(b, sizeof b, "B %c%.1f", st.balanceDb < 0 ? 'L' : 'R', std::fabs(st.balanceDb));
        c.text(X, Y + 53, b);
        return;
    }

    const int side = r.h - 2;
    imager.draw(c, r.x + 1, r.y + 1, side, f.newL, f.newR, f.newCount, f.dt);
    c.text(r.x + 4, r.y + 3, "L", 1, kMid);
    c.text(r.x + side - 6, r.y + 3, "R", 1, kMid);
    c.text(r.x + side / 2 + 3, r.y + 3, "M", 1, kMid);
    c.text(r.x + side - 6, r.y + side / 2 + 3, "S", 1, kMid);

    const int X = r.x + side + 8, W = r.x + r.w - X - 3, Y = r.y;
    c.vline(r.x + side + 3, Y, r.h, kDim);

    c.text(X, Y + 3, "CORRELATION", 1, kMid);
    std::snprintf(b, sizeof b, "%+.2f", st.correlation);
    c.text(X + W - Canvas::textWidth(b, 2), Y + 13, b, 2);
    cbar(c, X, Y + 31, W, 8, st.correlation, 1.0f);
    c.text(X, Y + 42, "-1", 1, kDim);
    c.text(X + W - 12, Y + 42, "+1", 1, kDim);

    c.text(X, Y + 56, "WIDTH", 1, kMid);
    std::snprintf(b, sizeof b, "%d%%", static_cast<int>(std::lround(st.width * 100)));
    c.text(X + W - Canvas::textWidth(b), Y + 56, b);
    hbar(c, X, Y + 66, W, 7, st.width, 0.0f, 1.5f);

    c.text(X, Y + 80, "BALANCE", 1, kMid);
    if (std::fabs(st.balanceDb) < 0.1f) std::snprintf(b, sizeof b, "C");
    else std::snprintf(b, sizeof b, "%c %.1f", st.balanceDb < 0 ? 'L' : 'R', std::fabs(st.balanceDb));
    c.text(X + W - Canvas::textWidth(b), Y + 80, b);
    cbar(c, X, Y + 90, W, 7, st.balanceDb, 6.0f);

    c.text(X, Y + 106, imager.autoGain ? "AUTO GAIN" : "FIXED GAIN", 1, kDim);
}

// ---------------------------------------------------------------- Scope

void drawScope(Canvas& c, Rect r, const FrameData& f, bool compact) {
    const int y0 = r.y + (compact ? 9 : 0);
    const int laneH = (r.y + r.h - y0 - 1) / 2;
    const float* chans[2] = {f.scopeL, f.scopeR};
    const int n = f.scopeCount;

    for (int ch = 0; ch < 2; ++ch) {
        const int ly = y0 + ch * (laneH + 1);
        const int mid = ly + laneH / 2;
        const float amp = laneH / 2.0f - 1.0f;
        dottedH(c, r.x, mid, r.w, 90);
        dottedH(c, r.x, ly + 1, r.w, kDim, 4);
        dottedH(c, r.x, ly + laneH - 2, r.w, kDim, 4);
        if (!compact) c.text(r.x + 2, ly + 2, ch == 0 ? "L" : "R", 1, kMid);
        if (ch == 0) dottedH(c, r.x, ly + laneH, r.w, kDim, 1);

        const float* s = chans[ch];
        if (!s || n < 2) continue;
        for (int x = 0; x < r.w; ++x) {
            // Min/max over the column's samples, overlapping one sample for continuity.
            const int i0 = static_cast<int>(static_cast<long long>(x) * (n - 1) / r.w);
            const int i1 = std::max(i0 + 1, static_cast<int>(static_cast<long long>(x + 1) * (n - 1) / r.w));
            float lo = s[i0], hi = s[i0];
            for (int i = i0 + 1; i <= std::min(i1, n - 1); ++i) {
                lo = std::min(lo, s[i]);
                hi = std::max(hi, s[i]);
            }
            const int yHi = mid - static_cast<int>(std::lround(std::clamp(hi, -1.0f, 1.0f) * amp));
            const int yLo = mid - static_cast<int>(std::lround(std::clamp(lo, -1.0f, 1.0f) * amp));
            c.vline(r.x + x, yHi, yLo - yHi + 1);
        }
    }
}

} // namespace mixviz
