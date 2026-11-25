#pragma once
#include <array>
#include <cmath>
#include <complex>
#include <cstddef>
#include <deque>
#include <vector>

namespace mixviz {

constexpr float kNegInf = -200.0f;

// Rolling history of the most recent stereo samples (main thread only).
class History {
public:
    explicit History(size_t capacityPow2 = 1u << 16);
    void push(float l, float r);
    // Copies the latest n frames (oldest first). n must be <= capacity.
    void latest(size_t n, float* l, float* r) const;
    size_t capacity() const { return cap_; }

private:
    std::vector<float> l_, r_;
    size_t cap_, mask_, w_ = 0;
};

// ITU-R BS.1770-4 / EBU R128 loudness: momentary (400 ms), short-term (3 s),
// gated integrated loudness, loudness range, and sample peak.
class LoudnessMeter {
public:
    void reset(int sampleRate);
    void resetIntegration(); // keeps filters, clears I / LRA / max peak
    void process(float l, float r);

    float momentary() const { return momentary_; }
    float shortTerm() const { return shortTerm_; }
    float integrated() const { return integrated_; }
    float range() const { return lra_; }
    float maxPeakDb() const { return maxPeakDb_; }
    float maxMomentary() const { return maxMomentary_; }
    // Short-term loudness sampled every 100 ms (newest at back), for graphs.
    const std::deque<float>& shortTermHistory() const { return stHistory_; }

    // Sample peak per channel since the last call (linear), then cleared.
    std::array<float, 2> takePeaks();

private:
    struct Biquad {
        double b0 = 1, b1 = 0, b2 = 0, a1 = 0, a2 = 0, z1 = 0, z2 = 0;
        double run(double x) {
            const double y = b0 * x + z1;
            z1 = b1 * x - a1 * y + z2;
            z2 = b2 * x - a2 * y;
            return y;
        }
    };
    void endBlock();
    void updateIntegrated();
    void updateRange();

    int sr_ = 48000;
    Biquad shelf_[2], highpass_[2];
    int blockLen_ = 4800, blockPos_ = 0;
    double blockAcc_ = 0.0;
    std::array<double, 30> blocks_{}; // last 3 s of 100 ms mean-square powers
    int blockCount_ = 0;
    std::vector<double> gatingPowers_; // 400 ms powers above absolute gate
    std::vector<double> stPowers_;     // 3 s powers above absolute gate
    std::deque<float> stHistory_;
    float momentary_ = kNegInf, shortTerm_ = kNegInf, integrated_ = kNegInf, lra_ = 0.0f;
    float maxPeakDb_ = kNegInf, maxMomentary_ = kNegInf;
    std::array<float, 2> peaks_{};
};

// 256 log-spaced bands from 20 Hz to 20 kHz with ballistics and peak hold.
class SpectrumAnalyzer {
public:
    static constexpr int kBands = 256;
    static constexpr int kFftSize = 8192;

    void reset(int sampleRate);
    // mono: latest kFftSize samples.
    void analyze(const float* mono, float dt);

    float band(int i) const { return bands_[i]; }
    float peak(int i) const { return peaks_[i]; }
    // Frequency at the lower edge of band position t (0..1 across the display).
    static float freqAt(float t);

    float tiltDbPerOct = 4.5f; // pink-noise compensation, 0 = flat

private:
    int sr_ = 48000;
    std::vector<float> window_;
    std::vector<std::complex<float>> buf_;
    std::vector<float> magDb_;
    std::array<float, kBands> bands_{}, peaks_{}, peakAge_{};
};

// Stereo-field statistics for the imager view.
struct StereoStats {
    float correlation = 0.0f;  // -1..+1, smoothed
    float balanceDb = 0.0f;    // + = right louder
    float width = 0.0f;        // side RMS / mid RMS
    void update(const float* l, const float* r, size_t n, float dt);
};

void fftInPlace(std::vector<std::complex<float>>& a);

inline float toDb(float lin) { return lin > 1e-10f ? 20.0f * std::log10(lin) : kNegInf; }

} // namespace mixviz
