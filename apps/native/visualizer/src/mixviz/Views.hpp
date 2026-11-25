#pragma once
#include "Canvas.hpp"
#include "Dsp.hpp"

#include <array>
#include <vector>

namespace mixviz {

struct Rect { int x, y, w, h; };

// Sample-peak bar ballistics with hold.
struct PeakMeter {
    std::array<float, 2> db{-120.0f, -120.0f}, hold{-120.0f, -120.0f}, holdAge{};
    void update(std::array<float, 2> linear, float dt);
};

// Vectorscope with phosphor persistence.
class Imager {
public:
    bool autoGain = true;
    void draw(lcd::Canvas& c, int x, int y, int side, const float* l, const float* r, size_t n, float dt);

private:
    std::vector<float> phosphor_;
    int side_ = 0;
    float env_ = 0.3f;
};

// Everything the views need for one frame.
struct FrameData {
    const LoudnessMeter* loudness = nullptr;
    const SpectrumAnalyzer* spectrum = nullptr;
    const StereoStats* stereo = nullptr;
    const PeakMeter* peaks = nullptr;
    const float* newL = nullptr; // samples that arrived this frame (imager)
    const float* newR = nullptr;
    size_t newCount = 0;
    const float* scopeL = nullptr; // triggered window
    const float* scopeR = nullptr;
    int scopeCount = 0;
    float dt = 0.0f;
};

void drawLoudness(lcd::Canvas& c, Rect r, const FrameData& f, bool compact);
void drawSpectrum(lcd::Canvas& c, Rect r, const FrameData& f, bool compact);
void drawImager(lcd::Canvas& c, Rect r, Imager& imager, const FrameData& f, bool compact);
void drawScope(lcd::Canvas& c, Rect r, const FrameData& f, bool compact);

} // namespace mixviz
