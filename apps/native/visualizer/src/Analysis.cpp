#include "Analysis.hpp"

#include <algorithm>
#include <cmath>

namespace fbvis {

using mixviz::SpectrumAnalyzer;

void Analysis::reset(int sampleRate) {
    sampleRate_ = sampleRate;
    history_ = mixviz::History();
    stereo_ = mixviz::StereoStats();
    peaks_ = mixviz::PeakMeter();
    imager_ = mixviz::Imager();
    if (sampleRate > 0) {
        loudness_.reset(sampleRate);
        spectrum_.reset(sampleRate);
    }
    silentFor_ = 1e9f;
}

void Analysis::resetLoudness() {
    loudness_.resetIntegration();
    peaks_ = mixviz::PeakMeter();
}

void Analysis::push(const float* left, const float* right, size_t frames) {
    if (consumed_) {
        newL_.clear();
        newR_.clear();
        consumed_ = false;
    }
    for (size_t i = 0; i < frames; ++i) {
        const float l = left[i], r = right[i];
        history_.push(l, r);
        if (sampleRate_) loudness_.process(l, r);
        if (newL_.size() < 16384) {
            newL_.push_back(l);
            newR_.push_back(r);
        }
    }
    if (frames) silentFor_ = 0.0f;
}

const mixviz::FrameData& Analysis::step(float dt, float scopeMs) {
    frame_ = mixviz::FrameData{};
    if (consumed_) {
        newL_.clear();
        newR_.clear();
    }
    consumed_ = true;
    silentFor_ += newL_.empty() ? dt : 0.0f;
    if (!sampleRate_) return frame_;
    peaks_.update(loudness_.takePeaks(), dt);

    const size_t statN = std::min<size_t>(static_cast<size_t>(sampleRate_ / 10), history_.capacity());
    statL_.resize(statN);
    statR_.resize(statN);
    history_.latest(statN, statL_.data(), statR_.data());
    stereo_.update(statL_.data(), statR_.data(), statN, dt);

    fftL_.resize(SpectrumAnalyzer::kFftSize);
    fftR_.resize(SpectrumAnalyzer::kFftSize);
    mono_.resize(SpectrumAnalyzer::kFftSize);
    history_.latest(SpectrumAnalyzer::kFftSize, fftL_.data(), fftR_.data());
    for (int i = 0; i < SpectrumAnalyzer::kFftSize; ++i) mono_[i] = 0.5f * (fftL_[i] + fftR_[i]);
    spectrum_.analyze(mono_.data(), dt);

    // Scope: the latest rising zero crossing (mono) that still leaves a full
    // window; the signal must first dip below -h, so noise cannot retrigger.
    const int cap = static_cast<int>(history_.capacity());
    const int span = std::clamp(static_cast<int>(scopeMs * sampleRate_ / 1000.0f), 16, cap / 2);
    const int search = std::min(span, cap / 2);
    scopeL_.resize(static_cast<size_t>(span + search));
    scopeR_.resize(static_cast<size_t>(span + search));
    history_.latest(static_cast<size_t>(span + search), scopeL_.data(), scopeR_.data());
    float pk = 0.0f;
    for (int i = 0; i < span + search; ++i) pk = std::max(pk, std::fabs(scopeL_[i] + scopeR_[i]));
    const float h = 0.2f * pk;
    int start = search;
    bool armed = false;
    for (int t = 0; t <= search; ++t) {
        const float m = scopeL_[t] + scopeR_[t];
        if (m < -h) armed = true;
        else if (armed && m >= 0.0f) {
            start = t;
            armed = false;
        }
    }

    frame_.loudness = &loudness_;
    frame_.spectrum = &spectrum_;
    frame_.stereo = &stereo_;
    frame_.peaks = &peaks_;
    frame_.newL = newL_.data();
    frame_.newR = newR_.data();
    frame_.newCount = newL_.size();
    frame_.scopeL = scopeL_.data() + start;
    frame_.scopeR = scopeR_.data() + start;
    frame_.scopeCount = span;
    frame_.dt = dt;
    // The views draw this frame's new samples; the next push starts a
    // fresh batch.
    return frame_;
}

} // namespace fbvis
