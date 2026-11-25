#include "LcdRenderer.hpp"

#include <algorithm>
#include <cmath>

namespace lcd {

const std::vector<Theme>& builtinThemes() {
    static const std::vector<Theme> themes = {
        // name               off (background)        on (pixel)              gap
        {"Ice negative",     {0.02f, 0.03f, 0.05f},  {0.86f, 0.93f, 1.00f},  {0.00f, 0.01f, 0.02f}},
        {"Blue STN",         {0.06f, 0.16f, 0.78f},  {0.92f, 0.95f, 1.00f},  {0.04f, 0.10f, 0.55f}},
        {"FSTN positive",    {0.80f, 0.82f, 0.80f},  {0.08f, 0.10f, 0.12f},  {0.70f, 0.72f, 0.70f}},
        {"Yellow-green STN", {0.62f, 0.72f, 0.16f},  {0.10f, 0.16f, 0.06f},  {0.54f, 0.63f, 0.12f}},
        {"Amber",            {0.05f, 0.02f, 0.00f},  {1.00f, 0.66f, 0.16f},  {0.02f, 0.01f, 0.00f}},
        {"Red negative",     {0.04f, 0.00f, 0.00f},  {1.00f, 0.18f, 0.12f},  {0.02f, 0.00f, 0.00f}},
    };
    return themes;
}

LcdRenderer::LcdRenderer(SDL_Renderer* renderer, int width, int height)
    : ren_(renderer), w_(width), h_(height),
      state_(static_cast<size_t>(width) * height, 0.0f),
      theme_(builtinThemes().front()) {}

LcdRenderer::~LcdRenderer() {
    if (tex_) SDL_DestroyTexture(tex_);
}

void LcdRenderer::resize(int width, int height) {
    if (width == w_ && height == h_) return;
    w_ = width;
    h_ = height;
    state_.assign(static_cast<size_t>(width) * height, 0.0f);
    scale_ = 0; // force texture rebuild on next draw
}

void LcdRenderer::update(const Canvas& canvas, float dt) {
    const uint8_t* src = canvas.data();
    const size_t n = std::min(state_.size(), static_cast<size_t>(canvas.width()) * canvas.height());
    // Exponential approach = first-order LCD response. Turning off is a bit slower
    // than turning on, as on real passive-matrix panels.
    const float aOn = tau_ > 0.0f ? 1.0f - std::exp(-dt / tau_) : 1.0f;
    const float aOff = tau_ > 0.0f ? 1.0f - std::exp(-dt / (tau_ * 1.6f)) : 1.0f;
    for (size_t i = 0; i < n; ++i) {
        float target = src[i] * (1.0f / 255.0f);
        float& s = state_[i];
        s += (target - s) * (target > s ? aOn : aOff);
    }
}

void LcdRenderer::rebuild(int scale) {
    if (tex_) SDL_DestroyTexture(tex_);
    scale_ = scale;
    const int tw = w_ * scale, th = h_ * scale;
    tex_ = SDL_CreateTexture(ren_, SDL_PIXELFORMAT_XRGB8888, SDL_TEXTUREACCESS_STREAMING, tw, th);
    SDL_SetTextureScaleMode(tex_, SDL_SCALEMODE_NEAREST);
    rgba_.assign(static_cast<size_t>(tw) * th, 0);

    // Separable backlight falloff: brighter middle, dimmer edges/corners (edge-lit panel).
    rowGain_.resize(th);
    colGain_.resize(tw);
    for (int y = 0; y < th; ++y) {
        float v = (y + 0.5f) / th * 2.0f - 1.0f;
        rowGain_[y] = 1.0f - 0.10f * v * v;
    }
    for (int x = 0; x < tw; ++x) {
        float u = (x + 0.5f) / tw * 2.0f - 1.0f;
        colGain_[x] = 1.0f - 0.08f * u * u * u * u;
    }
}

static inline uint32_t pack(float r, float g, float b) {
    auto c = [](float v) { return static_cast<uint32_t>(std::clamp(v, 0.0f, 1.0f) * 255.0f + 0.5f); };
    return 0xFF000000u | (c(r) << 16) | (c(g) << 8) | c(b);
}

void LcdRenderer::compose() {
    const int S = scale_;
    const int tw = w_ * S;
    const bool gridOn = grid_ && S >= 3;
    const Rgb off = theme_.off, on = theme_.on, gap = theme_.gap;
    const float br = brightness_;

    for (int ly = 0; ly < h_; ++ly) {
        for (int lx = 0; lx < w_; ++lx) {
            const float v = state_[static_cast<size_t>(ly) * w_ + lx];
            const float r = off.r + (on.r - off.r) * v;
            const float g = off.g + (on.g - off.g) * v;
            const float b = off.b + (on.b - off.b) * v;
            // Gap pixels mostly show the panel background, slightly influenced by the cell.
            const float gr = gap.r + (r - gap.r) * 0.25f;
            const float gg = gap.g + (g - gap.g) * 0.25f;
            const float gb = gap.b + (b - gap.b) * 0.25f;

            for (int sy = 0; sy < S; ++sy) {
                const int oy = ly * S + sy;
                const float rg = rowGain_[oy] * br;
                uint32_t* row = &rgba_[static_cast<size_t>(oy) * tw + lx * S];
                const bool gapRow = gridOn && sy == S - 1;
                for (int sx = 0; sx < S; ++sx) {
                    const float k = rg * colGain_[lx * S + sx];
                    const bool isGap = gapRow || (gridOn && sx == S - 1);
                    row[sx] = isGap ? pack(gr * k, gg * k, gb * k) : pack(r * k, g * k, b * k);
                }
            }
        }
    }
    SDL_UpdateTexture(tex_, nullptr, rgba_.data(), tw * static_cast<int>(sizeof(uint32_t)));
}

void LcdRenderer::draw(const SDL_FRect& area) {
    if (fill_) {
        // With the dot grid: compose cells at >= 3x and filter down.
        // Without it: 1:1 texture, pixel-art scaling keeps the font blocky and sharp.
        const int s = grid_ ? std::max(3, static_cast<int>(std::ceil(std::max(area.w / w_, area.h / h_)))) : 1;
        if (s != scale_ || !tex_) rebuild(s);
        SDL_SetTextureScaleMode(tex_, grid_ ? SDL_SCALEMODE_LINEAR : SDL_SCALEMODE_PIXELART);
        compose();
        SDL_RenderTexture(ren_, tex_, nullptr, &area);
        return;
    }

    // Reserve room for the bezel (in LCD pixels) when picking the scale.
    const float fit = std::min(area.w / (w_ + 2 * bezel_), area.h / (h_ + 2 * bezel_));
    float drawScale;
    int s;
    if (integerScale_) {
        s = std::max(1, static_cast<int>(std::floor(fit)));
        drawScale = static_cast<float>(s);
    } else {
        drawScale = std::max(fit, 0.25f);
        s = grid_ ? std::max(3, static_cast<int>(std::ceil(drawScale))) : 1; // >= 3 keeps the pixel grid
    }
    if (s != scale_ || !tex_) rebuild(s);
    SDL_ScaleMode mode = SDL_SCALEMODE_NEAREST;
    if (drawScale != static_cast<float>(s)) mode = grid_ ? SDL_SCALEMODE_LINEAR : SDL_SCALEMODE_PIXELART;
    SDL_SetTextureScaleMode(tex_, mode);
    compose();

    const float lw = std::round(w_ * drawScale), lh = std::round(h_ * drawScale);
    const SDL_FRect lcd{std::floor(area.x + (area.w - lw) / 2), std::floor(area.y + (area.h - lh) / 2), lw, lh};

    // Bezel: outer housing lip, then black glass margin around the active area.
    const float b = bezel_ * drawScale;
    const SDL_FRect lip{lcd.x - b, lcd.y - b, lcd.w + 2 * b, lcd.h + 2 * b};
    SDL_SetRenderDrawColor(ren_, 38, 38, 42, 255);
    SDL_RenderFillRect(ren_, &lip);
    const float g = b * 0.6f;
    const SDL_FRect glass{lcd.x - g, lcd.y - g, lcd.w + 2 * g, lcd.h + 2 * g};
    const Rgb& off = theme_.off;
    SDL_SetRenderDrawColor(ren_, static_cast<Uint8>(off.r * 0.35f * 255), static_cast<Uint8>(off.g * 0.35f * 255),
                           static_cast<Uint8>(off.b * 0.35f * 255), 255);
    SDL_RenderFillRect(ren_, &glass);

    SDL_RenderTexture(ren_, tex_, nullptr, &lcd);
}

bool LcdRenderer::saveBMP(const std::string& path) const {
    if (rgba_.empty()) return false;
    const int tw = w_ * scale_, th = h_ * scale_;
    SDL_Surface* surf = SDL_CreateSurfaceFrom(tw, th, SDL_PIXELFORMAT_XRGB8888,
                                              const_cast<uint32_t*>(rgba_.data()), tw * 4);
    if (!surf) return false;
    const bool ok = SDL_SaveBMP(surf, path.c_str());
    SDL_DestroySurface(surf);
    return ok;
}

} // namespace lcd
