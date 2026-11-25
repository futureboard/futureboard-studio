#include "Canvas.hpp"
#include "Font5x7.hpp"

#include <algorithm>
#include <cstdlib>

namespace lcd {

Canvas::Canvas(int width, int height)
    : w_(width), h_(height), px_(static_cast<size_t>(width) * height, 0) {}

void Canvas::clear(uint8_t v) { std::fill(px_.begin(), px_.end(), v); }

void Canvas::pixel(int x, int y, uint8_t v) {
    if (x < 0 || y < 0 || x >= w_ || y >= h_) return;
    px_[static_cast<size_t>(y) * w_ + x] = v;
}

uint8_t Canvas::get(int x, int y) const {
    if (x < 0 || y < 0 || x >= w_ || y >= h_) return 0;
    return px_[static_cast<size_t>(y) * w_ + x];
}

void Canvas::hline(int x, int y, int len, uint8_t v) {
    for (int i = 0; i < len; ++i) pixel(x + i, y, v);
}

void Canvas::vline(int x, int y, int len, uint8_t v) {
    for (int i = 0; i < len; ++i) pixel(x, y + i, v);
}

void Canvas::line(int x0, int y0, int x1, int y1, uint8_t v) {
    int dx = std::abs(x1 - x0), sx = x0 < x1 ? 1 : -1;
    int dy = -std::abs(y1 - y0), sy = y0 < y1 ? 1 : -1;
    int err = dx + dy;
    for (;;) {
        pixel(x0, y0, v);
        if (x0 == x1 && y0 == y1) break;
        int e2 = 2 * err;
        if (e2 >= dy) { err += dy; x0 += sx; }
        if (e2 <= dx) { err += dx; y0 += sy; }
    }
}

void Canvas::rect(int x, int y, int w, int h, uint8_t v) {
    if (w <= 0 || h <= 0) return;
    hline(x, y, w, v);
    hline(x, y + h - 1, w, v);
    vline(x, y, h, v);
    vline(x + w - 1, y, h, v);
}

void Canvas::fillRect(int x, int y, int w, int h, uint8_t v) {
    int x0 = std::max(x, 0), y0 = std::max(y, 0);
    int x1 = std::min(x + w, w_), y1 = std::min(y + h, h_);
    for (int yy = y0; yy < y1; ++yy)
        std::fill_n(px_.begin() + static_cast<size_t>(yy) * w_ + x0, std::max(0, x1 - x0), v);
}

void Canvas::invertRect(int x, int y, int w, int h) {
    int x0 = std::max(x, 0), y0 = std::max(y, 0);
    int x1 = std::min(x + w, w_), y1 = std::min(y + h, h_);
    for (int yy = y0; yy < y1; ++yy)
        for (int xx = x0; xx < x1; ++xx) {
            auto& p = px_[static_cast<size_t>(yy) * w_ + xx];
            p = static_cast<uint8_t>(255 - p);
        }
}

void Canvas::circle(int cx, int cy, int r, uint8_t v) {
    int x = r, y = 0, err = 1 - r;
    while (x >= y) {
        pixel(cx + x, cy + y, v); pixel(cx + y, cy + x, v);
        pixel(cx - y, cy + x, v); pixel(cx - x, cy + y, v);
        pixel(cx - x, cy - y, v); pixel(cx - y, cy - x, v);
        pixel(cx + y, cy - x, v); pixel(cx + x, cy - y, v);
        ++y;
        if (err < 0) err += 2 * y + 1;
        else { --x; err += 2 * (y - x) + 1; }
    }
}

void Canvas::fillCircle(int cx, int cy, int r, uint8_t v) {
    for (int dy = -r; dy <= r; ++dy)
        for (int dx = -r; dx <= r; ++dx)
            if (dx * dx + dy * dy <= r * r + r) pixel(cx + dx, cy + dy, v);
}

int Canvas::text(int x, int y, std::string_view s, int scale, uint8_t v) {
    int cx = x;
    for (char c : s) {
        const uint8_t* g = font5x7::glyph(c);
        for (int col = 0; col < font5x7::kWidth; ++col)
            for (int row = 0; row < font5x7::kHeight; ++row)
                if (g[col] & (1u << row))
                    fillRect(cx + col * scale, y + row * scale, scale, scale, v);
        cx += 6 * scale;
    }
    return cx - x;
}

int Canvas::textCentered(int cx, int y, std::string_view s, int scale, uint8_t v) {
    return text(cx - textWidth(s, scale) / 2, y, s, scale, v);
}

int Canvas::textWidth(std::string_view s, int scale) {
    // Last char has no trailing spacing column.
    return s.empty() ? 0 : static_cast<int>(s.size()) * 6 * scale - scale;
}

} // namespace lcd
