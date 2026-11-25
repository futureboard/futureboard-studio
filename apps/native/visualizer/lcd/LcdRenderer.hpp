#pragma once
#include "Canvas.hpp"

#include <SDL3/SDL.h>
#include <cstdint>
#include <string>
#include <vector>

namespace lcd {

struct Rgb { float r, g, b; };

struct Theme {
    const char* name;
    Rgb off;    // pixel fully off (shows backlight / background)
    Rgb on;     // pixel fully on
    Rgb gap;    // inter-pixel gap tint
};

const std::vector<Theme>& builtinThemes();

// Turns a Canvas into a physically-flavored LCD image:
// integer-scaled pixel cells, visible pixel grid, slow LCD response (ghosting),
// uneven backlight, bezel.
class LcdRenderer {
public:
    LcdRenderer(SDL_Renderer* renderer, int width, int height);
    ~LcdRenderer();
    LcdRenderer(const LcdRenderer&) = delete;
    LcdRenderer& operator=(const LcdRenderer&) = delete;

    void setTheme(const Theme& t) { theme_ = t; }
    const Theme& theme() const { return theme_; }

    void setGrid(bool on) { grid_ = on; }
    bool grid() const { return grid_; }

    // LCD response time constant in seconds; 0 disables ghosting.
    void setResponseTime(float tau) { tau_ = tau; }
    float responseTime() const { return tau_; }

    void setBrightness(float b) { brightness_ = b < 0.1f ? 0.1f : (b > 1.5f ? 1.5f : b); }
    float brightness() const { return brightness_; }

    // Integer scaling keeps every LCD pixel crisp. Fractional scaling fills the area
    // better: the cells are composed at a larger integer scale and filtered down.
    void setIntegerScale(bool on) { integerScale_ = on; }

    // Fill the whole draw area edge to edge (no bezel, no centering margins).
    // The caller should keep the canvas aspect close to the area's to avoid distortion.
    void setFill(bool on) { fill_ = on; }

    // Change the LCD resolution (resets the simulated crystal state).
    void resize(int width, int height);
    int width() const { return w_; }

    // Bezel thickness in LCD pixels.
    void setBezel(float lcdPixels) { bezel_ = lcdPixels < 0 ? 0 : lcdPixels; }

    // Advance the simulated liquid-crystal state toward the canvas contents.
    void update(const Canvas& canvas, float dt);

    // Draw bezel + LCD centered in `area`, using the largest integer scale that fits.
    void draw(const SDL_FRect& area);

    // Save the last composed LCD image (with grid/backlight) as BMP.
    bool saveBMP(const std::string& path) const;

    int scale() const { return scale_; }

private:
    void rebuild(int scale);
    void compose();

    SDL_Renderer* ren_;
    SDL_Texture* tex_ = nullptr;
    int w_, h_;
    int scale_ = 0;
    std::vector<float> state_;       // simulated crystal state, 0..1
    std::vector<uint32_t> rgba_;     // composed output, (w*scale) x (h*scale)
    std::vector<float> rowGain_, colGain_;
    Theme theme_;
    bool grid_ = true;
    bool integerScale_ = true;
    bool fill_ = false;
    float bezel_ = 6.0f;
    float tau_ = 0.045f;
    float brightness_ = 1.0f;
};

} // namespace lcd
