#pragma once
#include <cstdint>
#include <string_view>
#include <vector>

namespace lcd {

// Monochrome-ish framebuffer. Each pixel is an intensity 0 (off) .. 255 (fully on),
// so grayscale/dimmed pixels are possible, like LCD controllers with gray levels.
class Canvas {
public:
    Canvas(int width = 256, int height = 128);

    int width() const { return w_; }
    int height() const { return h_; }
    const uint8_t* data() const { return px_.data(); }

    void clear(uint8_t v = 0);
    void pixel(int x, int y, uint8_t v = 255);
    uint8_t get(int x, int y) const;

    void hline(int x, int y, int len, uint8_t v = 255);
    void vline(int x, int y, int len, uint8_t v = 255);
    void line(int x0, int y0, int x1, int y1, uint8_t v = 255);
    void rect(int x, int y, int w, int h, uint8_t v = 255);
    void fillRect(int x, int y, int w, int h, uint8_t v = 255);
    void invertRect(int x, int y, int w, int h);
    void circle(int cx, int cy, int r, uint8_t v = 255);
    void fillCircle(int cx, int cy, int r, uint8_t v = 255);

    // 5x7 font, advance 6*scale per char. Returns drawn width in pixels.
    int text(int x, int y, std::string_view s, int scale = 1, uint8_t v = 255);
    int textCentered(int cx, int y, std::string_view s, int scale = 1, uint8_t v = 255);
    static int textWidth(std::string_view s, int scale = 1);
    static int textHeight(int scale = 1) { return 7 * scale; }

private:
    int w_, h_;
    std::vector<uint8_t> px_;
};

} // namespace lcd
