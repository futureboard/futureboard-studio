#pragma once
// One stereo signal's analysis (loudness, spectrum, stereo field, peaks,
// triggered scope), fed from a link ring. The app runs one for the master
// and one for the requested track.

#include "mixviz/Dsp.hpp"
#include "mixviz/Views.hpp"

#include <vector>

namespace fbvis {

class Analysis {
public:
    // Clears every measurement and restarts at `sampleRate` (0: no audio).
    void reset(int sampleRate);
    int sampleRate() const { return sampleRate_; }

    // Appends samples that arrived this frame.
    void push(const float* left, const float* right, size_t frames);

    // Runs the per-frame analysis and returns what the views draw. Valid
    // until the next push/step. Without a sample rate the result is empty
    // (`loudness == nullptr`).
    const mixviz::FrameData& step(float dt, float scopeMs);

    void resetLoudness();
    mixviz::SpectrumAnalyzer& spectrum() { return spectrum_; }
    mixviz::Imager& imager() { return imager_; }
    // Whether audio arrived in the last second or so.
    bool live() const { return silentFor_ < 1.0f; }

private:
    int sampleRate_ = 0;
    float silentFor_ = 1e9f;
    bool consumed_ = false; // the batch in newL_/newR_ was handed to the views
    mixviz::History history_;
    mixviz::LoudnessMeter loudness_;
    mixviz::SpectrumAnalyzer spectrum_;
    mixviz::StereoStats stereo_;
    mixviz::PeakMeter peaks_;
    mixviz::Imager imager_;
    mixviz::FrameData frame_;
    std::vector<float> newL_, newR_;
    std::vector<float> fftL_, fftR_, mono_;
    std::vector<float> scopeL_, scopeR_, statL_, statR_;
};

} // namespace fbvis
