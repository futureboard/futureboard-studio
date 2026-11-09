//! Cabinet, loudspeaker and microphone simulation.
//!
//! This is intentionally an allocation-free IIR/modal implementation rather
//! than pretending a four-filter chain is an IR. Each cabinet family selects a
//! distinct acoustic topology (open-back cancellation, small-box mode,
//! multi-cone closed box, oversized low modes, or bass clean-low radiation).
//! A separate mic stage models capsule type, cone position, proximity, air
//! absorption and a subtle early reflection. Delay storage is allocated only
//! in construction/sample-rate preparation, never in `process`.

use super::smooth::Smoothed;
use super::{CabModel, MicModel};

const CONTROL_SMOOTH_SECONDS: f32 = 0.015;
const MODEL_FADE_SECONDS: f32 = 0.015;
const MAX_DELAY_SECONDS: f32 = 0.035;

#[inline]
fn finite(x: f32) -> f32 {
    if x.is_finite() {
        super::flush_denormal(x.clamp(-6.0, 6.0))
    } else {
        0.0
    }
}

#[inline]
fn pole(freq: f32, sample_rate: f32) -> f32 {
    (-std::f32::consts::TAU * freq.clamp(1.0, sample_rate * 0.45) / sample_rate.max(1.0)).exp()
}

#[inline]
fn alpha(freq: f32, sample_rate: f32) -> f32 {
    1.0 - pole(freq, sample_rate)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CabTopology {
    VintageClosed,
    OpenCombo,
    SmallCombo,
    ModernClosed,
    OpenBack,
    VintagePair,
    Oversized,
    Bass,
}

#[derive(Debug, Clone, Copy)]
struct CabProfile {
    topology: CabTopology,
    hpf_hz: f32,
    resonance_hz: f32,
    resonance_damping: f32,
    resonance_gain: f32,
    second_mode_hz: f32,
    second_mode_gain: f32,
    body_hz: f32,
    body_gain: f32,
    breakup_hz: f32,
    breakup_damping: f32,
    breakup_gain: f32,
    notch_hz: f32,
    notch_gain: f32,
    cutoff_hz: f32,
    slope: usize,
    rear_delay_ms: f32,
    rear_gain: f32,
    compression: f32,
    output_gain: f32,
}

impl CabProfile {
    fn for_model(model: CabModel) -> Self {
        match model {
            CabModel::Vintage4x12 => Self {
                topology: CabTopology::VintageClosed,
                hpf_hz: 72.0,
                resonance_hz: 108.0,
                resonance_damping: 0.44,
                resonance_gain: 0.42,
                second_mode_hz: 215.0,
                second_mode_gain: 0.16,
                body_hz: 620.0,
                body_gain: 0.18,
                breakup_hz: 2_750.0,
                breakup_damping: 0.30,
                breakup_gain: 0.22,
                notch_hz: 4_350.0,
                notch_gain: 0.14,
                cutoff_hz: 5_050.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.14,
                output_gain: 0.91,
            },
            CabModel::American2x12 => Self {
                topology: CabTopology::OpenCombo,
                hpf_hz: 88.0,
                resonance_hz: 135.0,
                resonance_damping: 0.58,
                resonance_gain: 0.26,
                second_mode_hz: 310.0,
                second_mode_gain: -0.12,
                body_hz: 760.0,
                body_gain: 0.10,
                breakup_hz: 3_350.0,
                breakup_damping: 0.38,
                breakup_gain: 0.18,
                notch_hz: 2_100.0,
                notch_gain: 0.08,
                cutoff_hz: 6_050.0,
                slope: 2,
                rear_delay_ms: 1.15,
                rear_gain: -0.18,
                compression: 0.07,
                output_gain: 0.94,
            },
            CabModel::Tweed1x12 => Self {
                topology: CabTopology::SmallCombo,
                hpf_hz: 103.0,
                resonance_hz: 168.0,
                resonance_damping: 0.38,
                resonance_gain: 0.50,
                second_mode_hz: 410.0,
                second_mode_gain: 0.20,
                body_hz: 940.0,
                body_gain: 0.25,
                breakup_hz: 2_150.0,
                breakup_damping: 0.48,
                breakup_gain: 0.16,
                notch_hz: 3_250.0,
                notch_gain: 0.18,
                cutoff_hz: 4_150.0,
                slope: 4,
                rear_delay_ms: 0.72,
                rear_gain: -0.10,
                compression: 0.18,
                output_gain: 0.88,
            },
            CabModel::Modern4x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 66.0,
                resonance_hz: 96.0,
                resonance_damping: 0.68,
                resonance_gain: 0.29,
                second_mode_hz: 188.0,
                second_mode_gain: 0.09,
                body_hz: 540.0,
                body_gain: -0.12,
                breakup_hz: 3_650.0,
                breakup_damping: 0.52,
                breakup_gain: 0.25,
                notch_hz: 2_250.0,
                notch_gain: 0.16,
                cutoff_hz: 6_600.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.11,
                output_gain: 0.90,
            },
            CabModel::OpenBack => Self {
                topology: CabTopology::OpenBack,
                hpf_hz: 92.0,
                resonance_hz: 142.0,
                resonance_damping: 0.66,
                resonance_gain: 0.22,
                second_mode_hz: 360.0,
                second_mode_gain: -0.15,
                body_hz: 720.0,
                body_gain: 0.08,
                breakup_hz: 3_100.0,
                breakup_damping: 0.44,
                breakup_gain: 0.15,
                notch_hz: 1_850.0,
                notch_gain: 0.10,
                cutoff_hz: 5_850.0,
                slope: 2,
                rear_delay_ms: 1.65,
                rear_gain: -0.26,
                compression: 0.05,
                output_gain: 0.96,
            },
            CabModel::Vintage2x12 => Self {
                topology: CabTopology::VintagePair,
                hpf_hz: 78.0,
                resonance_hz: 122.0,
                resonance_damping: 0.48,
                resonance_gain: 0.38,
                second_mode_hz: 245.0,
                second_mode_gain: 0.20,
                body_hz: 680.0,
                body_gain: 0.16,
                breakup_hz: 2_550.0,
                breakup_damping: 0.34,
                breakup_gain: 0.24,
                notch_hz: 3_750.0,
                notch_gain: 0.17,
                cutoff_hz: 4_750.0,
                slope: 3,
                rear_delay_ms: 0.36,
                rear_gain: 0.08,
                compression: 0.13,
                output_gain: 0.92,
            },
            CabModel::Oversized4x12 => Self {
                topology: CabTopology::Oversized,
                hpf_hz: 55.0,
                resonance_hz: 82.0,
                resonance_damping: 0.50,
                resonance_gain: 0.48,
                second_mode_hz: 156.0,
                second_mode_gain: 0.25,
                body_hz: 470.0,
                body_gain: -0.08,
                breakup_hz: 3_300.0,
                breakup_damping: 0.48,
                breakup_gain: 0.21,
                notch_hz: 2_050.0,
                notch_gain: 0.18,
                cutoff_hz: 5_750.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.20,
                output_gain: 0.87,
            },
            CabModel::BassCabinet => Self {
                topology: CabTopology::Bass,
                hpf_hz: 34.0,
                resonance_hz: 68.0,
                resonance_damping: 0.64,
                resonance_gain: 0.31,
                second_mode_hz: 138.0,
                second_mode_gain: 0.14,
                body_hz: 410.0,
                body_gain: 0.08,
                breakup_hz: 2_200.0,
                breakup_damping: 0.58,
                breakup_gain: 0.08,
                notch_hz: 1_250.0,
                notch_gain: 0.08,
                cutoff_hz: 4_850.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.08,
                output_gain: 0.97,
            },
            // British stack: forward upper mids, pronounced ~3.4 kHz breakup,
            // early high roll-off — the classic mid-honk closed 4x12.
            CabModel::Brit4x12 => Self {
                topology: CabTopology::VintageClosed,
                hpf_hz: 78.0,
                resonance_hz: 116.0,
                resonance_damping: 0.42,
                resonance_gain: 0.40,
                second_mode_hz: 230.0,
                second_mode_gain: 0.18,
                body_hz: 820.0,
                body_gain: 0.30,
                breakup_hz: 3_400.0,
                breakup_damping: 0.28,
                breakup_gain: 0.30,
                notch_hz: 4_800.0,
                notch_gain: 0.20,
                cutoff_hz: 4_650.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.13,
                output_gain: 0.90,
            },
            // Uberkab: tight German high-gain — scooped low-mids, controlled
            // resonance, extended top for percussive djent attack.
            CabModel::Uber4x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 74.0,
                resonance_hz: 98.0,
                resonance_damping: 0.72,
                resonance_gain: 0.27,
                second_mode_hz: 205.0,
                second_mode_gain: -0.18,
                body_hz: 560.0,
                body_gain: -0.20,
                breakup_hz: 4_050.0,
                breakup_damping: 0.55,
                breakup_gain: 0.27,
                notch_hz: 2_150.0,
                notch_gain: 0.20,
                cutoff_hz: 6_950.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.12,
                output_gain: 0.89,
            },
            // SLO Custom: smooth, singing high-gain — full but tight lows,
            // rounded upper mids, gentle top so leads stay creamy not harsh.
            CabModel::Slo4x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 68.0,
                resonance_hz: 104.0,
                resonance_damping: 0.58,
                resonance_gain: 0.34,
                second_mode_hz: 198.0,
                second_mode_gain: 0.10,
                body_hz: 640.0,
                body_gain: 0.14,
                breakup_hz: 3_150.0,
                breakup_damping: 0.46,
                breakup_gain: 0.20,
                notch_hz: 2_650.0,
                notch_gain: 0.12,
                cutoff_hz: 5_450.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.14,
                output_gain: 0.90,
            },
            // Not a modeled voicing: the IR engine (`dsp::ir`) owns this
            // selection and the modeled lane is faded out while it is
            // active. A profile is still needed so the standby lane holds
            // something valid — the most neutral voicing in the set.
            CabModel::Ir => Self::for_model(CabModel::Vintage4x12),
            // Modern 2x12: the closed, scooped, extended-top family shared
            // with Modern/Uber/Slo 4x12, scaled down — a higher box
            // resonance and a milder scoop than any of the 4x12s.
            CabModel::Modern2x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 78.0,
                resonance_hz: 112.0,
                resonance_damping: 0.62,
                resonance_gain: 0.30,
                second_mode_hz: 220.0,
                second_mode_gain: 0.05,
                body_hz: 580.0,
                body_gain: -0.06,
                breakup_hz: 3_500.0,
                breakup_damping: 0.50,
                breakup_gain: 0.22,
                notch_hz: 2_300.0,
                notch_gain: 0.14,
                cutoff_hz: 6_800.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.10,
                output_gain: 0.91,
            },
            // American 1x12 Combo: `OpenBack` family, but a single small
            // speaker — higher resonance, brighter and more extended top,
            // a shorter/weaker rear path than the 2-speaker Open Back.
            CabModel::American1x12 => Self {
                topology: CabTopology::OpenBack,
                hpf_hz: 105.0,
                resonance_hz: 165.0,
                resonance_damping: 0.55,
                resonance_gain: 0.30,
                second_mode_hz: 400.0,
                second_mode_gain: -0.08,
                body_hz: 800.0,
                body_gain: 0.05,
                breakup_hz: 3_600.0,
                breakup_damping: 0.40,
                breakup_gain: 0.17,
                notch_hz: 2_000.0,
                notch_gain: 0.09,
                cutoff_hz: 7_200.0,
                slope: 2,
                rear_delay_ms: 0.85,
                rear_gain: -0.16,
                compression: 0.05,
                output_gain: 0.95,
            },
            // Open alnico pair: a higher bell than the American 2x12 and
            // a lighter rear reflection.
            CabModel::Alnico2x12 => Self {
                topology: CabTopology::OpenCombo,
                hpf_hz: 85.0,
                resonance_hz: 120.0,
                resonance_damping: 0.48,
                resonance_gain: 0.22,
                second_mode_hz: 720.0,
                second_mode_gain: 0.18,
                body_hz: 2_400.0,
                body_gain: 0.28,
                breakup_hz: 4_800.0,
                breakup_damping: 0.35,
                breakup_gain: 0.22,
                notch_hz: 3_200.0,
                notch_gain: 0.04,
                cutoff_hz: 6_200.0,
                slope: 3,
                rear_delay_ms: 1.1,
                rear_gain: -0.12,
                compression: 0.04,
                output_gain: 0.98,
            },
            // Closed greenback stack: the mid honk sits lower and the top
            // dies earlier than the British Stack.
            CabModel::Green4x12 => Self {
                topology: CabTopology::VintageClosed,
                hpf_hz: 90.0,
                resonance_hz: 110.0,
                resonance_damping: 0.42,
                resonance_gain: 0.34,
                second_mode_hz: 1_100.0,
                second_mode_gain: 0.26,
                body_hz: 1_800.0,
                body_gain: 0.32,
                breakup_hz: 2_800.0,
                breakup_damping: 0.5,
                breakup_gain: 0.08,
                notch_hz: 4_000.0,
                notch_gain: -0.12,
                cutoff_hz: 4_200.0,
                slope: 2,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.08,
                output_gain: 0.92,
            },
            // Paper-cone single: boxier and darker than the American 1x12.
            CabModel::Jensen1x12 => Self {
                topology: CabTopology::SmallCombo,
                hpf_hz: 95.0,
                resonance_hz: 140.0,
                resonance_damping: 0.6,
                resonance_gain: 0.18,
                second_mode_hz: 480.0,
                second_mode_gain: 0.1,
                body_hz: 1_600.0,
                body_gain: 0.12,
                breakup_hz: 3_200.0,
                breakup_damping: 0.45,
                breakup_gain: 0.2,
                notch_hz: 2_400.0,
                notch_gain: 0.06,
                cutoff_hz: 5_400.0,
                slope: 2,
                rear_delay_ms: 0.6,
                rear_gain: -0.08,
                compression: 0.06,
                output_gain: 0.9,
            },
            // Tight closed 4x12: more top than a greenback, less of the
            // modern scoop.
            CabModel::Cream4x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 80.0,
                resonance_hz: 95.0,
                resonance_damping: 0.55,
                resonance_gain: 0.2,
                second_mode_hz: 900.0,
                second_mode_gain: 0.08,
                body_hz: 2_200.0,
                body_gain: 0.1,
                breakup_hz: 5_000.0,
                breakup_damping: 0.32,
                breakup_gain: 0.16,
                notch_hz: 350.0,
                notch_gain: -0.06,
                cutoff_hz: 7_800.0,
                slope: 2,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.1,
                output_gain: 0.94,
            },
            // Deeper than the Bass Cabinet: the port sits lower and the
            // top is cut harder.
            CabModel::Bass810 => Self {
                topology: CabTopology::Bass,
                hpf_hz: 32.0,
                resonance_hz: 55.0,
                resonance_damping: 0.7,
                resonance_gain: 0.4,
                second_mode_hz: 180.0,
                second_mode_gain: 0.22,
                body_hz: 400.0,
                body_gain: 0.08,
                breakup_hz: 1_200.0,
                breakup_damping: 0.6,
                breakup_gain: -0.1,
                notch_hz: 2_500.0,
                notch_gain: -0.2,
                cutoff_hz: 3_200.0,
                slope: 2,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.12,
                output_gain: 1.05,
            },
            // Bright open single: a high bell and a longer rear bounce
            // than the American 1x12.
            CabModel::Silver1x12 => Self {
                topology: CabTopology::OpenBack,
                hpf_hz: 110.0,
                resonance_hz: 150.0,
                resonance_damping: 0.4,
                resonance_gain: 0.16,
                second_mode_hz: 900.0,
                second_mode_gain: 0.14,
                body_hz: 3_200.0,
                body_gain: 0.34,
                breakup_hz: 6_200.0,
                breakup_damping: 0.28,
                breakup_gain: 0.26,
                notch_hz: 1_400.0,
                notch_gain: 0.05,
                cutoff_hz: 5_800.0,
                slope: 2,
                rear_delay_ms: 1.4,
                rear_gain: -0.22,
                compression: 0.03,
                output_gain: 0.9,
            },
            // Small sealed single. The rolloff starts early and the box mode sits high.
            CabModel::Closed1x12 => Self {
                topology: CabTopology::VintageClosed,
                hpf_hz: 128.0,
                resonance_hz: 210.0,
                resonance_damping: 0.32,
                resonance_gain: 0.55,
                second_mode_hz: 480.0,
                second_mode_gain: 0.22,
                body_hz: 1_100.0,
                body_gain: 0.20,
                breakup_hz: 2_400.0,
                breakup_damping: 0.42,
                breakup_gain: 0.12,
                notch_hz: 3_100.0,
                notch_gain: 0.22,
                cutoff_hz: 3_400.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.16,
                output_gain: 0.86,
            },
            // A port under a pair: a low bump, then a tighter top than an open 2x12.
            CabModel::Ported2x12 => Self {
                topology: CabTopology::VintagePair,
                hpf_hz: 58.0,
                resonance_hz: 70.0,
                resonance_damping: 0.28,
                resonance_gain: 0.62,
                second_mode_hz: 190.0,
                second_mode_gain: 0.10,
                body_hz: 540.0,
                body_gain: 0.06,
                breakup_hz: 2_900.0,
                breakup_damping: 0.40,
                breakup_gain: 0.16,
                notch_hz: 1_600.0,
                notch_gain: 0.12,
                cutoff_hz: 4_400.0,
                slope: 3,
                rear_delay_ms: 2.4,
                rear_gain: 0.16,
                compression: 0.10,
                output_gain: 0.90,
            },
            // Four tens, open. The rear wave is stronger and later than a combo.
            CabModel::Open4x10 => Self {
                topology: CabTopology::OpenBack,
                hpf_hz: 98.0,
                resonance_hz: 160.0,
                resonance_damping: 0.55,
                resonance_gain: 0.18,
                second_mode_hz: 420.0,
                second_mode_gain: -0.18,
                body_hz: 880.0,
                body_gain: 0.04,
                breakup_hz: 3_800.0,
                breakup_damping: 0.36,
                breakup_gain: 0.20,
                notch_hz: 2_400.0,
                notch_gain: 0.08,
                cutoff_hz: 6_400.0,
                slope: 2,
                rear_delay_ms: 2.1,
                rear_gain: -0.42,
                compression: 0.04,
                output_gain: 0.93,
            },
            CabModel::Ceramic2x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 84.0,
                resonance_hz: 118.0,
                resonance_damping: 0.72,
                resonance_gain: 0.22,
                second_mode_hz: 260.0,
                second_mode_gain: 0.06,
                body_hz: 700.0,
                body_gain: -0.04,
                breakup_hz: 2_800.0,
                breakup_damping: 0.58,
                breakup_gain: 0.10,
                notch_hz: 3_600.0,
                notch_gain: 0.20,
                cutoff_hz: 4_200.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.12,
                output_gain: 0.88,
            },
            CabModel::Deep4x12 => Self {
                topology: CabTopology::Oversized,
                hpf_hz: 42.0,
                resonance_hz: 64.0,
                resonance_damping: 0.40,
                resonance_gain: 0.58,
                second_mode_hz: 120.0,
                second_mode_gain: 0.30,
                body_hz: 360.0,
                body_gain: -0.14,
                breakup_hz: 2_600.0,
                breakup_damping: 0.50,
                breakup_gain: 0.14,
                notch_hz: 1_700.0,
                notch_gain: 0.22,
                cutoff_hz: 4_600.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.22,
                output_gain: 0.84,
            },
            CabModel::Combo1x15 => Self {
                topology: CabTopology::OpenCombo,
                hpf_hz: 48.0,
                resonance_hz: 78.0,
                resonance_damping: 0.46,
                resonance_gain: 0.40,
                second_mode_hz: 190.0,
                second_mode_gain: -0.08,
                body_hz: 420.0,
                body_gain: 0.18,
                breakup_hz: 1_800.0,
                breakup_damping: 0.55,
                breakup_gain: 0.08,
                notch_hz: 2_600.0,
                notch_gain: 0.16,
                cutoff_hz: 3_200.0,
                slope: 3,
                rear_delay_ms: 2.6,
                rear_gain: -0.34,
                compression: 0.09,
                output_gain: 0.92,
            },
            CabModel::Alnico1x10 => Self {
                topology: CabTopology::SmallCombo,
                hpf_hz: 140.0,
                resonance_hz: 240.0,
                resonance_damping: 0.34,
                resonance_gain: 0.48,
                second_mode_hz: 620.0,
                second_mode_gain: 0.26,
                body_hz: 1_400.0,
                body_gain: 0.22,
                breakup_hz: 4_200.0,
                breakup_damping: 0.30,
                breakup_gain: 0.28,
                notch_hz: 2_200.0,
                notch_gain: 0.10,
                cutoff_hz: 5_200.0,
                slope: 3,
                rear_delay_ms: 0.55,
                rear_gain: -0.14,
                compression: 0.08,
                output_gain: 0.90,
            },
            CabModel::Dark15 => Self {
                topology: CabTopology::Bass,
                hpf_hz: 40.0,
                resonance_hz: 72.0,
                resonance_damping: 0.50,
                resonance_gain: 0.36,
                second_mode_hz: 150.0,
                second_mode_gain: 0.18,
                body_hz: 280.0,
                body_gain: 0.16,
                breakup_hz: 1_200.0,
                breakup_damping: 0.70,
                breakup_gain: 0.04,
                notch_hz: 2_800.0,
                notch_gain: 0.24,
                cutoff_hz: 2_400.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.14,
                output_gain: 0.95,
            },
            CabModel::Razor4x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 88.0,
                resonance_hz: 112.0,
                resonance_damping: 0.78,
                resonance_gain: 0.16,
                second_mode_hz: 240.0,
                second_mode_gain: 0.04,
                body_hz: 700.0,
                body_gain: -0.28,
                breakup_hz: 4_200.0,
                breakup_damping: 0.40,
                breakup_gain: 0.32,
                notch_hz: 1_800.0,
                notch_gain: 0.26,
                cutoff_hz: 7_200.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.16,
                output_gain: 0.86,
            },
            CabModel::HonkStack => Self {
                topology: CabTopology::VintageClosed,
                hpf_hz: 78.0,
                resonance_hz: 130.0,
                resonance_damping: 0.32,
                resonance_gain: 0.22,
                second_mode_hz: 280.0,
                second_mode_gain: 0.10,
                body_hz: 980.0,
                body_gain: 0.42,
                breakup_hz: 2_400.0,
                breakup_damping: 0.28,
                breakup_gain: 0.36,
                notch_hz: 4_800.0,
                notch_gain: 0.22,
                cutoff_hz: 4_200.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.18,
                output_gain: 0.84,
            },
            CabModel::Djent4x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 110.0,
                resonance_hz: 150.0,
                resonance_damping: 0.82,
                resonance_gain: 0.12,
                second_mode_hz: 320.0,
                second_mode_gain: -0.06,
                body_hz: 620.0,
                body_gain: -0.34,
                breakup_hz: 3_800.0,
                breakup_damping: 0.55,
                breakup_gain: 0.18,
                notch_hz: 1_500.0,
                notch_gain: 0.30,
                cutoff_hz: 6_200.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.10,
                output_gain: 0.88,
            },
            CabModel::SawStack => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 74.0,
                resonance_hz: 100.0,
                resonance_damping: 0.60,
                resonance_gain: 0.20,
                second_mode_hz: 210.0,
                second_mode_gain: 0.08,
                body_hz: 1_600.0,
                body_gain: 0.38,
                breakup_hz: 4_800.0,
                breakup_damping: 0.26,
                breakup_gain: 0.40,
                notch_hz: 800.0,
                notch_gain: 0.18,
                cutoff_hz: 7_800.0,
                slope: 2,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.12,
                output_gain: 0.82,
            },
            CabModel::Cannon4x12 => Self {
                topology: CabTopology::VintageClosed,
                hpf_hz: 70.0,
                resonance_hz: 104.0,
                resonance_damping: 0.48,
                resonance_gain: 0.34,
                second_mode_hz: 190.0,
                second_mode_gain: 0.18,
                body_hz: 820.0,
                body_gain: 0.30,
                breakup_hz: 2_900.0,
                breakup_damping: 0.34,
                breakup_gain: 0.28,
                notch_hz: 3_600.0,
                notch_gain: 0.10,
                cutoff_hz: 5_400.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.20,
                output_gain: 0.85,
            },
            CabModel::Ice4x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 120.0,
                resonance_hz: 180.0,
                resonance_damping: 0.70,
                resonance_gain: 0.10,
                second_mode_hz: 400.0,
                second_mode_gain: -0.16,
                body_hz: 900.0,
                body_gain: -0.22,
                breakup_hz: 5_200.0,
                breakup_damping: 0.32,
                breakup_gain: 0.34,
                notch_hz: 1_200.0,
                notch_gain: 0.20,
                cutoff_hz: 8_200.0,
                slope: 2,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.08,
                output_gain: 0.90,
            },
            CabModel::Doom810 => Self {
                topology: CabTopology::Bass,
                hpf_hz: 28.0,
                resonance_hz: 48.0,
                resonance_damping: 0.36,
                resonance_gain: 0.62,
                second_mode_hz: 96.0,
                second_mode_gain: 0.28,
                body_hz: 220.0,
                body_gain: 0.24,
                breakup_hz: 900.0,
                breakup_damping: 0.62,
                breakup_gain: 0.06,
                notch_hz: 2_200.0,
                notch_gain: 0.28,
                cutoff_hz: 2_000.0,
                slope: 4,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.28,
                output_gain: 0.80,
            },
            CabModel::Tight2x12 => Self {
                topology: CabTopology::ModernClosed,
                hpf_hz: 96.0,
                resonance_hz: 140.0,
                resonance_damping: 0.74,
                resonance_gain: 0.18,
                second_mode_hz: 300.0,
                second_mode_gain: 0.02,
                body_hz: 760.0,
                body_gain: -0.18,
                breakup_hz: 3_400.0,
                breakup_damping: 0.46,
                breakup_gain: 0.22,
                notch_hz: 1_900.0,
                notch_gain: 0.16,
                cutoff_hz: 5_800.0,
                slope: 3,
                rear_delay_ms: 0.0,
                rear_gain: 0.0,
                compression: 0.14,
                output_gain: 0.89,
            },
        }
    }
}

/// Rear-wave contribution.
///
/// A negative `rear_gain` is an open back: lows cancel, highs stay as a
/// dull remainder. A positive gain is a reflection, mixed full-band.
fn rear_term(
    channel: &mut CabChannel,
    delay: &DelayLine,
    sample_rate: f32,
    profile: CabProfile,
) -> f32 {
    if profile.rear_delay_ms <= 0.0 || profile.rear_gain.abs() < 1.0e-4 {
        return 0.0;
    }
    let rear = delay.read(sample_rate * profile.rear_delay_ms * 0.001);
    if profile.rear_gain < 0.0 {
        let a = alpha(340.0, sample_rate);
        channel.rear_low = finite(channel.rear_low + a * (rear - channel.rear_low));
        let low = channel.rear_low;
        let high = rear - low;
        low * profile.rear_gain * 1.9 + high * profile.rear_gain * 0.25
    } else {
        rear * profile.rear_gain
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Resonator {
    low: f32,
    band: f32,
}

impl Resonator {
    #[inline]
    fn tick(&mut self, x: f32, f: f32, damping: f32) -> f32 {
        let high = x - self.low - damping.clamp(0.08, 1.8) * self.band;
        self.band = finite(self.band + f * high);
        self.low = finite(self.low + f * self.band);
        self.band
    }
}

#[derive(Debug, Clone)]
struct CabChannel {
    hp_x: f32,
    hp_y: f32,
    mode1: Resonator,
    mode2: Resonator,
    breakup: Resonator,
    notch: Resonator,
    body_low: f32,
    proximity_low: f32,
    lpf: [f32; 4],
    dynamic_mid: f32,
    ribbon_low: f32,
    condenser_low: f32,
    room_low: f32,
    envelope: f32,
    /// Extra closed-back pole, below the speaker stack.
    closed_lp: f32,
    /// Low band of the rear wave, for open-back cancellation.
    rear_low: f32,
    /// High band used to pull the plastic 5 kHz edge down.
    presence: f32,
}

impl Default for CabChannel {
    fn default() -> Self {
        Self {
            hp_x: 0.0,
            hp_y: 0.0,
            mode1: Resonator::default(),
            mode2: Resonator::default(),
            breakup: Resonator::default(),
            notch: Resonator::default(),
            body_low: 0.0,
            proximity_low: 0.0,
            lpf: [0.0; 4],
            dynamic_mid: 0.0,
            ribbon_low: 0.0,
            condenser_low: 0.0,
            room_low: 0.0,
            envelope: 0.0,
            closed_lp: 0.0,
            rear_low: 0.0,
            presence: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
struct DelayLine {
    data: Vec<f32>,
    write: usize,
}

impl DelayLine {
    fn new(sample_rate: f32) -> Self {
        Self {
            data: vec![0.0; (sample_rate * MAX_DELAY_SECONDS).ceil() as usize + 4],
            write: 0,
        }
    }

    fn resize(&mut self, sample_rate: f32) {
        self.data
            .resize((sample_rate * MAX_DELAY_SECONDS).ceil() as usize + 4, 0.0);
        self.clear();
    }

    fn clear(&mut self) {
        self.data.fill(0.0);
        self.write = 0;
    }

    #[inline]
    fn read(&self, delay_samples: f32) -> f32 {
        let delay = delay_samples.clamp(1.0, (self.data.len() - 2) as f32);
        let mut position = self.write as f32 - delay;
        while position < 0.0 {
            position += self.data.len() as f32;
        }
        let base = position.floor() as usize % self.data.len();
        let next = (base + 1) % self.data.len();
        let fraction = position - position.floor();
        self.data[base] * (1.0 - fraction) + self.data[next] * fraction
    }

    #[inline]
    fn write(&mut self, x: f32) {
        self.data[self.write] = x;
        self.write += 1;
        if self.write >= self.data.len() {
            self.write = 0;
        }
    }
}

#[derive(Debug, Clone)]
struct CabLane {
    sample_rate: f32,
    model: CabModel,
    profile: CabProfile,
    left: CabChannel,
    right: CabChannel,
    delay_l: DelayLine,
    delay_r: DelayLine,
    hp_pole: f32,
    body_alpha: f32,
    mode1_f: f32,
    mode2_f: f32,
    breakup_f: f32,
    notch_f: f32,
    proximity_alpha: f32,
    dynamic_alpha: f32,
    ribbon_alpha: f32,
    condenser_alpha: f32,
    position: Smoothed,
    distance: Smoothed,
    mic_kind: Smoothed,
    speaker_lpf_alpha: Smoothed,
    air_alpha: Smoothed,
    room_delay: Smoothed,
    room_mix: Smoothed,
    level: Smoothed,
}

#[derive(Debug, Clone, Copy)]
struct CabControls {
    position: f32,
    distance: f32,
    mic_kind: f32,
    speaker_lpf_alpha: f32,
    air_alpha: f32,
    room_delay: f32,
    room_mix: f32,
    level: f32,
    mode1_f: f32,
    mode2_f: f32,
    breakup_f: f32,
    notch_f: f32,
    proximity_alpha: f32,
    dynamic_alpha: f32,
    ribbon_alpha: f32,
    condenser_alpha: f32,
}

impl CabLane {
    fn new(sample_rate: f32, model: CabModel) -> Self {
        let sr = sample_rate.max(1.0);
        let profile = CabProfile::for_model(model);
        let mut lane = Self {
            sample_rate: sr,
            model,
            profile,
            left: CabChannel::default(),
            right: CabChannel::default(),
            delay_l: DelayLine::new(sr),
            delay_r: DelayLine::new(sr),
            hp_pole: pole(profile.hpf_hz, sr),
            body_alpha: alpha(profile.body_hz, sr),
            mode1_f: 0.0,
            mode2_f: 0.0,
            breakup_f: 0.0,
            notch_f: 0.0,
            proximity_alpha: 0.0,
            dynamic_alpha: 0.0,
            ribbon_alpha: 0.0,
            condenser_alpha: 0.0,
            position: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, 0.2),
            distance: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, 0.4),
            mic_kind: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, 0.0),
            speaker_lpf_alpha: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, alpha(5_000.0, sr)),
            air_alpha: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, alpha(8_000.0, sr)),
            room_delay: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, sr * 0.008),
            room_mix: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, 0.04),
            level: Smoothed::new(sr, CONTROL_SMOOTH_SECONDS, 1.0),
        };
        lane.configure(model, MicModel::Dynamic, 20.0, 40.0);
        lane.snap_controls();
        lane
    }

    fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.delay_l.resize(self.sample_rate);
        self.delay_r.resize(self.sample_rate);
        for value in [
            &mut self.position,
            &mut self.distance,
            &mut self.mic_kind,
            &mut self.speaker_lpf_alpha,
            &mut self.air_alpha,
            &mut self.room_delay,
            &mut self.room_mix,
            &mut self.level,
        ] {
            value.set_time(self.sample_rate, CONTROL_SMOOTH_SECONDS);
        }
        self.prepare_coefficients();
    }

    fn prepare_coefficients(&mut self) {
        self.hp_pole = pole(self.profile.hpf_hz, self.sample_rate);
        self.body_alpha = alpha(self.profile.body_hz, self.sample_rate);
        let resonator_f = |frequency: f32| {
            (2.0 * (std::f32::consts::PI * frequency / self.sample_rate).sin()).clamp(0.0, 0.95)
        };
        self.mode1_f = resonator_f(self.profile.resonance_hz);
        self.mode2_f = resonator_f(self.profile.second_mode_hz);
        self.breakup_f = resonator_f(self.profile.breakup_hz);
        self.notch_f = resonator_f(self.profile.notch_hz);
        self.proximity_alpha = alpha(170.0, self.sample_rate);
        self.dynamic_alpha = alpha(2_600.0, self.sample_rate);
        self.ribbon_alpha = alpha(3_650.0, self.sample_rate);
        self.condenser_alpha = alpha(7_600.0, self.sample_rate);
    }

    fn configure(&mut self, model: CabModel, mic: MicModel, position: f32, distance: f32) {
        self.model = model;
        self.profile = CabProfile::for_model(model);
        self.prepare_coefficients();
        let p = (position / 100.0).clamp(0.0, 1.0);
        let d = (distance / 100.0).clamp(0.0, 1.0);
        self.position.set_target(p);
        self.distance.set_target(d);
        self.mic_kind.set_target(mic.index() as f32);

        // Edge placement changes the breakup contribution, radiation cutoff
        // and cancellation depth together. Distance additionally adds air
        // absorption and early-room energy.
        let capsule_extension = match mic {
            MicModel::Dynamic => 0.90,
            MicModel::Ribbon => 0.68,
            MicModel::Condenser => 1.16,
        };
        let speaker_cutoff = self.profile.cutoff_hz * (1.12 - p * 0.32);
        self.speaker_lpf_alpha
            .set_target(alpha(speaker_cutoff, self.sample_rate));
        let air_cutoff = (speaker_cutoff * capsule_extension * (1.0 - d * 0.58))
            .clamp(2_200.0, self.sample_rate * 0.45);
        self.air_alpha
            .set_target(alpha(air_cutoff, self.sample_rate));
        self.room_delay
            .set_target(self.sample_rate * (0.0065 + d * 0.014));
        self.room_mix.set_target(0.018 + d * 0.095);
        self.level
            .set_target((1.0 - d * 0.13) * self.profile.output_gain);
    }

    fn snap_controls(&mut self) {
        for value in [
            &mut self.position,
            &mut self.distance,
            &mut self.mic_kind,
            &mut self.speaker_lpf_alpha,
            &mut self.air_alpha,
            &mut self.room_delay,
            &mut self.room_mix,
            &mut self.level,
        ] {
            value.snap();
        }
    }

    fn reset(&mut self) {
        self.left = CabChannel::default();
        self.right = CabChannel::default();
        self.delay_l.clear();
        self.delay_r.clear();
        self.snap_controls();
    }

    #[inline]
    fn controls(&mut self) -> CabControls {
        CabControls {
            position: self.position.tick(),
            distance: self.distance.tick(),
            mic_kind: self.mic_kind.tick(),
            speaker_lpf_alpha: self.speaker_lpf_alpha.tick(),
            air_alpha: self.air_alpha.tick(),
            room_delay: self.room_delay.tick(),
            room_mix: self.room_mix.tick(),
            level: self.level.tick(),
            mode1_f: self.mode1_f,
            mode2_f: self.mode2_f,
            breakup_f: self.breakup_f,
            notch_f: self.notch_f,
            proximity_alpha: self.proximity_alpha,
            dynamic_alpha: self.dynamic_alpha,
            ribbon_alpha: self.ribbon_alpha,
            condenser_alpha: self.condenser_alpha,
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn process_channel(
        channel: &mut CabChannel,
        delay: &mut DelayLine,
        input: f32,
        profile: CabProfile,
        sample_rate: f32,
        hp_pole: f32,
        body_alpha: f32,
        controls: CabControls,
    ) -> f32 {
        let hp = input - channel.hp_x + hp_pole * channel.hp_y;
        channel.hp_x = input;
        channel.hp_y = finite(hp);

        let mode1 = channel
            .mode1
            .tick(channel.hp_y, controls.mode1_f, profile.resonance_damping);
        let mode2 = channel.mode2.tick(
            channel.hp_y,
            controls.mode2_f,
            (profile.resonance_damping + 0.18).min(1.5),
        );
        channel.body_low =
            finite(channel.body_low + body_alpha * (channel.hp_y - channel.body_low));
        let body = channel.hp_y - channel.body_low;
        let breakup =
            channel
                .breakup
                .tick(channel.hp_y, controls.breakup_f, profile.breakup_damping);
        let notch = channel.notch.tick(channel.hp_y, controls.notch_f, 0.72);

        let center = 1.0 - controls.position;
        let mut acoustic = match profile.topology {
            CabTopology::VintageClosed => {
                channel.hp_y
                    + mode1 * profile.resonance_gain
                    + mode2 * profile.second_mode_gain
                    + body * profile.body_gain
                    + breakup * profile.breakup_gain * (0.45 + center * 0.75)
                    - notch * profile.notch_gain
            }
            CabTopology::OpenCombo | CabTopology::OpenBack => {
                let rear = rear_term(channel, delay, sample_rate, profile);
                channel.hp_y
                    + mode1 * profile.resonance_gain
                    + body * profile.body_gain
                    + rear
                    + breakup * profile.breakup_gain * (0.35 + center * 0.70)
            }
            CabTopology::SmallCombo => {
                let rear = rear_term(channel, delay, sample_rate, profile);
                channel.hp_y
                    + mode1 * profile.resonance_gain
                    + mode2 * profile.second_mode_gain
                    + body * profile.body_gain
                    + rear
                    - notch * profile.notch_gain
            }
            CabTopology::ModernClosed => {
                channel.hp_y
                    + mode1 * profile.resonance_gain
                    + mode2 * profile.second_mode_gain
                    + body * profile.body_gain
                    + breakup * profile.breakup_gain * (0.60 + center * 0.82)
                    - notch * profile.notch_gain * (0.7 + controls.position * 0.5)
            }
            CabTopology::VintagePair => {
                let pair = rear_term(channel, delay, sample_rate, profile);
                channel.hp_y
                    + mode1 * profile.resonance_gain
                    + mode2 * profile.second_mode_gain
                    + pair
                    + body * profile.body_gain
                    + breakup * profile.breakup_gain * (0.55 + center * 0.65)
                    - notch * profile.notch_gain
            }
            CabTopology::Oversized => {
                channel.hp_y
                    + mode1 * profile.resonance_gain
                    + mode2 * profile.second_mode_gain
                    + channel.body_low * 0.08
                    + body * profile.body_gain
                    + breakup * profile.breakup_gain * (0.50 + center * 0.70)
                    - notch * profile.notch_gain
            }
            CabTopology::Bass => {
                channel.hp_y
                    + mode1 * profile.resonance_gain
                    + mode2 * profile.second_mode_gain
                    + channel.body_low * 0.18
                    + body * profile.body_gain
                    + breakup * profile.breakup_gain * center
            }
        };

        channel.envelope = finite(channel.envelope + 0.002 * (acoustic.abs() - channel.envelope));
        acoustic /= 1.0 + channel.envelope * profile.compression;

        // Natural loudspeaker radiation loss: two to four cascaded poles.
        for stage in channel.lpf.iter_mut().take(profile.slope) {
            *stage = finite(*stage + controls.speaker_lpf_alpha * (acoustic - *stage));
            acoustic = *stage;
        }

        // A closed back rolls off sooner than the open cabinets. One more
        // pole, under the speaker stack, keeps the sealed boxes from sharing
        // the open boxes' top.
        if matches!(
            profile.topology,
            CabTopology::VintageClosed
                | CabTopology::ModernClosed
                | CabTopology::Oversized
                | CabTopology::Bass
        ) {
            let closed = alpha((profile.cutoff_hz * 0.55).max(900.0), sample_rate);
            channel.closed_lp = finite(channel.closed_lp + closed * (acoustic - channel.closed_lp));
            acoustic = channel.closed_lp;
        }

        // A stack of one-poles leaves a hard edge around 5 kHz. Pull a
        // little of that band down so the cone does not sound like plastic.
        let presence = alpha(5_200.0, sample_rate);
        channel.presence = finite(channel.presence + presence * (acoustic - channel.presence));
        acoustic -= (acoustic - channel.presence) * 0.18;

        // Position changes proximity and cone breakup as well as high
        // radiation. Distance reduces proximity and introduces a softened,
        // delayed early-room reflection.
        channel.proximity_low = finite(
            channel.proximity_low + controls.proximity_alpha * (acoustic - channel.proximity_low),
        );
        let proximity =
            channel.proximity_low * (1.0 - controls.distance) * (0.36 - controls.position * 0.10);
        let close = acoustic + proximity;

        // Three capsule topologies, continuously crossfaded for automation.
        channel.dynamic_mid =
            finite(channel.dynamic_mid + controls.dynamic_alpha * (close - channel.dynamic_mid));
        let dynamic = close
            + (close - channel.dynamic_mid) * (0.13 + center * 0.12)
            + breakup * center * 0.035;
        channel.ribbon_low =
            finite(channel.ribbon_low + controls.ribbon_alpha * (close - channel.ribbon_low));
        let ribbon = channel.ribbon_low + channel.proximity_low * 0.15 * (1.0 - controls.distance);
        channel.condenser_low = finite(
            channel.condenser_low + controls.condenser_alpha * (close - channel.condenser_low),
        );
        let condenser = close + (close - channel.condenser_low) * 0.10 + breakup * center * 0.025;
        let mic = if controls.mic_kind <= 1.0 {
            dynamic * (1.0 - controls.mic_kind) + ribbon * controls.mic_kind
        } else {
            let blend = controls.mic_kind - 1.0;
            ribbon * (1.0 - blend) + condenser * blend
        };

        channel.room_low = finite(channel.room_low + controls.air_alpha * (mic - channel.room_low));
        let direct = channel.room_low;
        let reflection = delay.read(controls.room_delay);
        let softened_reflection = reflection * (1.0 - controls.position * 0.18);
        delay.write(finite(acoustic));
        finite((direct + softened_reflection * controls.room_mix) * controls.level)
    }

    #[inline]
    fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let controls = self.controls();
        (
            Self::process_channel(
                &mut self.left,
                &mut self.delay_l,
                left,
                self.profile,
                self.sample_rate,
                self.hp_pole,
                self.body_alpha,
                controls,
            ),
            Self::process_channel(
                &mut self.right,
                &mut self.delay_r,
                right,
                self.profile,
                self.sample_rate,
                self.hp_pole,
                self.body_alpha,
                controls,
            ),
        )
    }
}

#[derive(Debug, Clone)]
pub(super) struct Cabinet {
    sample_rate: f32,
    active: CabLane,
    standby: CabLane,
    switching: bool,
    switch_position: f32,
    switch_step: f32,
}

impl Cabinet {
    pub(super) fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(1.0);
        Self {
            sample_rate: sr,
            active: CabLane::new(sr, CabModel::Vintage4x12),
            standby: CabLane::new(sr, CabModel::Vintage4x12),
            switching: false,
            switch_position: 0.0,
            switch_step: 1.0 / (sr * MODEL_FADE_SECONDS).max(1.0),
        }
    }

    pub(super) fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.switch_step = 1.0 / (self.sample_rate * MODEL_FADE_SECONDS).max(1.0);
        self.active.set_sample_rate(self.sample_rate);
        self.standby.set_sample_rate(self.sample_rate);
    }

    pub(super) fn reset(&mut self) {
        // State restore is normally followed by reset before playback. Adopt
        // the prepared lane so reset never silently reverts the selected cab.
        if self.switching {
            std::mem::swap(&mut self.active, &mut self.standby);
        }
        self.active.reset();
        self.standby.reset();
        self.switching = false;
        self.switch_position = 0.0;
    }

    /// Position and distance retain their existing 0..100 wire semantics:
    /// position 0=center / 100=edge, distance 0=close / 100=far.
    pub(super) fn configure(
        &mut self,
        model: CabModel,
        mic: MicModel,
        position: f32,
        distance: f32,
    ) {
        if model == self.active.model && !self.switching {
            self.active.configure(model, mic, position, distance);
        } else if self.switching && model == self.standby.model {
            self.standby.configure(model, mic, position, distance);
        } else {
            self.standby.configure(model, mic, position, distance);
            self.standby.reset();
            self.switching = true;
            self.switch_position = 0.0;
        }
    }

    #[inline]
    pub(super) fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let old = self.active.process(left, right);
        if !self.switching {
            return old;
        }
        let new = self.standby.process(left, right);
        let p = self.switch_position.clamp(0.0, 1.0);
        let old_gain = (1.0 - p).sqrt();
        let new_gain = p.sqrt();
        let out = (
            finite(old.0 * old_gain + new.0 * new_gain),
            finite(old.1 * old_gain + new.1 * new_gain),
        );
        self.switch_position += self.switch_step;
        if self.switch_position >= 1.0 {
            std::mem::swap(&mut self.active, &mut self.standby);
            self.switching = false;
            self.switch_position = 0.0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_level(model: CabModel, mic: MicModel, position: f32, distance: f32, hz: f32) -> f32 {
        let mut cab = Cabinet::new(48_000.0);
        cab.configure(model, mic, position, distance);
        cab.reset();
        let mut energy = 0.0;
        for n in 0..16_000 {
            let x = (n as f32 * hz * std::f32::consts::TAU / 48_000.0).sin() * 0.25;
            let y = cab.process(x, x).0;
            assert!(y.is_finite());
            if n >= 8_000 {
                energy += y * y;
            }
        }
        (energy / 8_000.0).sqrt()
    }

    fn render(model: CabModel, mic: MicModel, position: f32, distance: f32) -> Vec<f32> {
        let mut cab = Cabinet::new(48_000.0);
        cab.configure(model, mic, position, distance);
        cab.reset();
        (0..16_000)
            .map(|n| {
                let x = (n as f32 * 0.071).sin() * 0.3 + (n as f32 * 0.193).sin() * 0.08;
                cab.process(x, x).0
            })
            .collect()
    }

    #[test]
    fn cabinets_band_limit_speaker_output() {
        // `MODELED`, not `ALL`: the IR selection has no filter profile of its
        // own — `dsp::ir` owns it, and its own tests cover it.
        for model in CabModel::MODELED {
            let mid = sine_level(*model, MicModel::Dynamic, 0.0, 0.0, 1_000.0);
            let fizz = sine_level(*model, MicModel::Dynamic, 0.0, 0.0, 12_000.0);
            assert!(mid > 0.002, "{model:?} muted the midrange");
            assert!(
                fizz < mid * 0.35,
                "{model:?} did not limit fizz: {fizz}/{mid}"
            );
        }
    }

    #[test]
    fn cabinet_families_and_microphones_are_distinct() {
        let cabinets: Vec<_> = CabModel::MODELED
            .iter()
            .map(|m| render(*m, MicModel::Dynamic, 30.0, 25.0))
            .collect();
        for i in 0..cabinets.len() {
            for j in (i + 1)..cabinets.len() {
                let diff = (cabinets[i]
                    .iter()
                    .skip(8_000)
                    .zip(cabinets[j].iter().skip(8_000))
                    .map(|(a, b)| (a - b).powi(2))
                    .sum::<f32>()
                    / 8_000.0)
                    .sqrt();
                assert!(
                    diff > 0.0005,
                    "{:?} == {:?}",
                    CabModel::MODELED[i],
                    CabModel::MODELED[j]
                );
            }
        }

        let dynamic = render(CabModel::Modern4x12, MicModel::Dynamic, 20.0, 20.0);
        let ribbon = render(CabModel::Modern4x12, MicModel::Ribbon, 20.0, 20.0);
        let condenser = render(CabModel::Modern4x12, MicModel::Condenser, 20.0, 20.0);
        for (a, b, names) in [
            (&dynamic, &ribbon, "dynamic/ribbon"),
            (&dynamic, &condenser, "dynamic/condenser"),
            (&ribbon, &condenser, "ribbon/condenser"),
        ] {
            let diff = (a
                .iter()
                .skip(8_000)
                .zip(b.iter().skip(8_000))
                .map(|(x, y)| (x - y).powi(2))
                .sum::<f32>()
                / 8_000.0)
                .sqrt();
            assert!(diff > 0.001, "{names} not distinct: {diff}");
        }
    }

    #[test]
    fn center_is_brighter_than_edge_and_distance_changes_more_than_level() {
        let center = sine_level(CabModel::Modern4x12, MicModel::Dynamic, 0.0, 10.0, 4_500.0);
        let edge = sine_level(
            CabModel::Modern4x12,
            MicModel::Dynamic,
            100.0,
            10.0,
            4_500.0,
        );
        assert!(center > edge * 1.08, "center={center} edge={edge}");

        let near_low = sine_level(CabModel::Vintage2x12, MicModel::Ribbon, 40.0, 0.0, 120.0);
        let far_low = sine_level(CabModel::Vintage2x12, MicModel::Ribbon, 40.0, 100.0, 120.0);
        let near_high = sine_level(CabModel::Vintage2x12, MicModel::Ribbon, 40.0, 0.0, 4_000.0);
        let far_high = sine_level(
            CabModel::Vintage2x12,
            MicModel::Ribbon,
            40.0,
            100.0,
            4_000.0,
        );
        assert!(
            (near_low / far_low.max(1.0e-6) - near_high / far_high.max(1.0e-6)).abs() > 0.05,
            "distance behaved as level only"
        );
    }

    #[test]
    fn switching_is_click_bounded_and_instances_are_isolated() {
        let mut switched = Cabinet::new(48_000.0);
        let mut untouched = switched.clone();
        let mut control = switched.clone();
        let mut previous = 0.0;
        let mut max_step = 0.0f32;
        for n in 0..24_000 {
            if n == 8_000 {
                switched.configure(CabModel::BassCabinet, MicModel::Condenser, 85.0, 75.0);
            }
            let x = (n as f32 * 0.051).sin() * 0.3;
            let y = switched.process(x, x).0;
            let a = untouched.process(x, x).0;
            let b = control.process(x, x).0;
            assert!((a - b).abs() < 1.0e-7, "instance state leaked");
            if n > 100 {
                max_step = max_step.max((y - previous).abs());
            }
            previous = y;
        }
        assert!(max_step < 0.30, "cab switch clicked: {max_step}");
    }
}
