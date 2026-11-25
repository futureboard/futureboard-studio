#include "Dsp.hpp"

#include <algorithm>
#include <cmath>
#include <numeric>

namespace mixviz {

constexpr double kPi = 3.14159265358979323846;

// ---------------------------------------------------------------- History

History::History(size_t capacityPow2)
    : l_(capacityPow2, 0.0f), r_(capacityPow2, 0.0f), cap_(capacityPow2), mask_(capacityPow2 - 1) {}

void History::push(float l, float r) {
    l_[w_ & mask_] = l;
    r_[w_ & mask_] = r;
    ++w_;
}

void History::latest(size_t n, float* l, float* r) const {
    n = std::min(n, cap_);
    const size_t start = w_ - n;
    for (size_t i = 0; i < n; ++i) {
        const size_t k = (start + i) & mask_;
        l[i] = l_[k];
        r[i] = r_[k];
    }
}

// ---------------------------------------------------------------- LoudnessMeter

static float powerToLufs(double p) {
    return p > 1e-20 ? static_cast<float>(-0.691 + 10.0 * std::log10(p)) : kNegInf;
}

static double lufsToPower(double lufs) { return std::pow(10.0, (lufs + 0.691) / 10.0); }

void LoudnessMeter::reset(int sampleRate) {
    sr_ = sampleRate;
    const double fs = sampleRate;

    // Stage 1: high shelf (head acoustics). Coefficients re-derived for any rate.
    {
        const double f0 = 1681.974450955533, G = 3.999843853973347, Q = 0.7071752369554196;
        const double K = std::tan(kPi * f0 / fs);
        const double Vh = std::pow(10.0, G / 20.0);
        const double Vb = std::pow(Vh, 0.4996667741545416);
        const double a0 = 1.0 + K / Q + K * K;
        Biquad b;
        b.b0 = (Vh + Vb * K / Q + K * K) / a0;
        b.b1 = 2.0 * (K * K - Vh) / a0;
        b.b2 = (Vh - Vb * K / Q + K * K) / a0;
        b.a1 = 2.0 * (K * K - 1.0) / a0;
        b.a2 = (1.0 - K / Q + K * K) / a0;
        shelf_[0] = shelf_[1] = b;
    }
    // Stage 2: RLB high-pass.
    {
        const double f0 = 38.13547087602444, Q = 0.5003270373238773;
        const double K = std::tan(kPi * f0 / fs);
        const double a0 = 1.0 + K / Q + K * K;
        Biquad b;
        b.b0 = 1.0; b.b1 = -2.0; b.b2 = 1.0;
        b.a1 = 2.0 * (K * K - 1.0) / a0;
        b.a2 = (1.0 - K / Q + K * K) / a0;
        highpass_[0] = highpass_[1] = b;
    }

    blockLen_ = std::max(1, sampleRate / 10);
    blockPos_ = 0;
    blockAcc_ = 0.0;
    blocks_.fill(0.0);
    blockCount_ = 0;
    momentary_ = shortTerm_ = kNegInf;
    stHistory_.clear();
    peaks_ = {};
    resetIntegration();
}

void LoudnessMeter::resetIntegration() {
    gatingPowers_.clear();
    stPowers_.clear();
    integrated_ = kNegInf;
    lra_ = 0.0f;
    maxPeakDb_ = kNegInf;
    maxMomentary_ = kNegInf;
}

void LoudnessMeter::process(float l, float r) {
    peaks_[0] = std::max(peaks_[0], std::fabs(l));
    peaks_[1] = std::max(peaks_[1], std::fabs(r));
    const double kl = highpass_[0].run(shelf_[0].run(l));
    const double kr = highpass_[1].run(shelf_[1].run(r));
    blockAcc_ += kl * kl + kr * kr; // L/R channel weights are 1.0
    if (++blockPos_ >= blockLen_) endBlock();
}

std::array<float, 2> LoudnessMeter::takePeaks() {
    const auto p = peaks_;
    maxPeakDb_ = std::max(maxPeakDb_, toDb(std::max(p[0], p[1])));
    peaks_ = {};
    return p;
}

void LoudnessMeter::endBlock() {
    blocks_[blockCount_ % blocks_.size()] = blockAcc_ / blockLen_;
    ++blockCount_;
    blockAcc_ = 0.0;
    blockPos_ = 0;

    auto meanLast = [&](int n) {
        n = std::min(n, blockCount_);
        double s = 0.0;
        for (int i = 1; i <= n; ++i) s += blocks_[(blockCount_ - i) % blocks_.size()];
        return n > 0 ? s / n : 0.0;
    };

    // Each 100 ms step yields a 400 ms window with 75 % overlap, as BS.1770 gating requires.
    if (blockCount_ >= 4) {
        const double pm = meanLast(4);
        momentary_ = powerToLufs(pm);
        maxMomentary_ = std::max(maxMomentary_, momentary_);
        if (momentary_ > -70.0f) gatingPowers_.push_back(pm);
        updateIntegrated();
    }
    if (blockCount_ >= 30) {
        const double ps = meanLast(30);
        shortTerm_ = powerToLufs(ps);
        if (shortTerm_ > -70.0f) stPowers_.push_back(ps);
        if (blockCount_ % 10 == 0) updateRange();
    } else {
        shortTerm_ = powerToLufs(meanLast(blockCount_));
    }
    stHistory_.push_back(shortTerm_);
    if (stHistory_.size() > 1200) stHistory_.pop_front(); // 2 minutes
}

void LoudnessMeter::updateIntegrated() {
    if (gatingPowers_.empty()) return;
    const double absMean = std::accumulate(gatingPowers_.begin(), gatingPowers_.end(), 0.0) / gatingPowers_.size();
    const double relGate = lufsToPower(powerToLufs(absMean) - 10.0);
    double sum = 0.0;
    size_t n = 0;
    for (double p : gatingPowers_)
        if (p >= relGate) { sum += p; ++n; }
    integrated_ = n ? powerToLufs(sum / n) : kNegInf;
}

void LoudnessMeter::updateRange() {
    if (stPowers_.size() < 2) return;
    const double absMean = std::accumulate(stPowers_.begin(), stPowers_.end(), 0.0) / stPowers_.size();
    const double relGate = lufsToPower(powerToLufs(absMean) - 20.0);
    std::vector<float> v;
    v.reserve(stPowers_.size());
    for (double p : stPowers_)
        if (p >= relGate) v.push_back(powerToLufs(p));
    if (v.size() < 2) return;
    std::sort(v.begin(), v.end());
    auto pct = [&](double q) { return v[static_cast<size_t>(std::lround(q * (v.size() - 1)))]; };
    lra_ = pct(0.95) - pct(0.10);
}

// ---------------------------------------------------------------- FFT

void fftInPlace(std::vector<std::complex<float>>& a) {
    const size_t n = a.size();
    for (size_t i = 1, j = 0; i < n; ++i) {
        size_t bit = n >> 1;
        for (; j & bit; bit >>= 1) j ^= bit;
        j ^= bit;
        if (i < j) std::swap(a[i], a[j]);
    }
    for (size_t len = 2; len <= n; len <<= 1) {
        const double ang = -2.0 * kPi / static_cast<double>(len);
        const std::complex<float> wl(static_cast<float>(std::cos(ang)), static_cast<float>(std::sin(ang)));
        for (size_t i = 0; i < n; i += len) {
            std::complex<float> w(1.0f, 0.0f);
            for (size_t k = 0; k < len / 2; ++k) {
                const auto u = a[i + k];
                const auto v = a[i + k + len / 2] * w;
                a[i + k] = u + v;
                a[i + k + len / 2] = u - v;
                w *= wl;
            }
        }
    }
}

// ---------------------------------------------------------------- SpectrumAnalyzer

float SpectrumAnalyzer::freqAt(float t) { return 20.0f * std::pow(1000.0f, t); }

void SpectrumAnalyzer::reset(int sampleRate) {
    sr_ = sampleRate;
    window_.resize(kFftSize);
    for (int i = 0; i < kFftSize; ++i)
        window_[i] = static_cast<float>(0.5 - 0.5 * std::cos(2.0 * kPi * i / (kFftSize - 1)));
    buf_.assign(kFftSize, {});
    magDb_.assign(kFftSize / 2 + 1, kNegInf);
    bands_.fill(-120.0f);
    peaks_.fill(-120.0f);
    peakAge_.fill(0.0f);
}

void SpectrumAnalyzer::analyze(const float* mono, float dt) {
    if (window_.empty()) reset(sr_);
    for (int i = 0; i < kFftSize; ++i) buf_[i] = {mono[i] * window_[i], 0.0f};
    fftInPlace(buf_);

    // Normalize so a full-scale sine reads 0 dB (Hann coherent gain = 0.5).
    const float norm = 2.0f / (kFftSize * 0.5f);
    for (int k = 0; k <= kFftSize / 2; ++k) magDb_[k] = toDb(std::abs(buf_[k]) * norm);

    const float binHz = static_cast<float>(sr_) / kFftSize;
    const int maxBin = kFftSize / 2;
    for (int b = 0; b < kBands; ++b) {
        const float f0 = freqAt(static_cast<float>(b) / kBands);
        const float f1 = freqAt(static_cast<float>(b + 1) / kBands);
        const float x0 = f0 / binHz, x1 = f1 / binHz;
        float db;
        if (x1 - x0 < 1.0f) {
            // Narrower than one bin: interpolate at band center.
            const float xc = std::min((x0 + x1) * 0.5f, static_cast<float>(maxBin - 1));
            const int k = static_cast<int>(xc);
            const float t = xc - k;
            db = magDb_[k] + (magDb_[k + 1] - magDb_[k]) * t;
        } else {
            db = kNegInf;
            for (int k = static_cast<int>(std::ceil(x0)); k <= std::min(static_cast<int>(x1), maxBin); ++k)
                db = std::max(db, magDb_[k]);
        }
        const float fc = std::sqrt(f0 * f1);
        db += tiltDbPerOct * std::log2(fc / 1000.0f);

        // Ballistics: instant attack, 36 dB/s release.
        bands_[b] = db > bands_[b] ? db : std::max(db, bands_[b] - 36.0f * dt);
        // Peak hold 1.2 s, then fall 20 dB/s.
        if (db >= peaks_[b]) { peaks_[b] = db; peakAge_[b] = 0.0f; }
        else if ((peakAge_[b] += dt) > 1.2f) peaks_[b] -= 20.0f * dt;
    }
}

// ---------------------------------------------------------------- StereoStats

void StereoStats::update(const float* l, const float* r, size_t n, float dt) {
    double ll = 0, rr = 0, lr = 0, mm = 0, ss = 0;
    for (size_t i = 0; i < n; ++i) {
        ll += l[i] * l[i];
        rr += r[i] * r[i];
        lr += l[i] * r[i];
        const double m = (l[i] + r[i]) * 0.5, s = (l[i] - r[i]) * 0.5;
        mm += m * m;
        ss += s * s;
    }
    const float a = 1.0f - std::exp(-dt / 0.25f); // 250 ms smoothing
    const bool hasSignal = ll + rr > 1e-9 * n;
    const float corr = hasSignal ? static_cast<float>(lr / std::sqrt(std::max(ll * rr, 1e-30))) : 0.0f;
    const float bal = hasSignal ? static_cast<float>(10.0 * std::log10((rr + 1e-12) / (ll + 1e-12))) : 0.0f;
    const float wid = mm > 1e-12 ? static_cast<float>(std::sqrt(ss / mm)) : (ss > 1e-12 ? 2.0f : 0.0f);
    correlation += (std::clamp(corr, -1.0f, 1.0f) - correlation) * a;
    balanceDb += (std::clamp(bal, -24.0f, 24.0f) - balanceDb) * a;
    width += (std::min(wid, 2.0f) - width) * a;
}

} // namespace mixviz
