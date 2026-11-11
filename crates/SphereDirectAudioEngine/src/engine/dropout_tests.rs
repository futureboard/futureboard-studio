use super::*;
use std::sync::atomic::Ordering;

// 512 frames @ 48 kHz: per-block budget = 512 * 1e6 / 48000 ≈ 10_666 µs.
const FRAMES: usize = 512;
const SR: u32 = 48_000;

#[test]
fn threshold_ratios_are_ordered() {
    // Stricter modes flag at a smaller fraction of the deadline.
    let frac = |m: DropoutProtectionMode| {
        let (n, d) = m.dropout_threshold_ratio();
        n as f64 / d as f64
    };
    assert!(frac(AudioguardLevel::High) < frac(AudioguardLevel::Normal));
    assert!(frac(AudioguardLevel::Normal) < frac(AudioguardLevel::Off));
    assert_eq!(frac(AudioguardLevel::Off), 1.0);
}

#[test]
fn fast_block_never_flags_dropout() {
    let shared = SharedState::default();
    shared
        .dropout_protection_mode
        .store(AudioguardLevel::Normal as u8, Ordering::Relaxed);
    record_output_callback_timing(&shared, 1_000, FRAMES, SR, true);
    assert_eq!(shared.dropout_count.load(Ordering::Relaxed), 0);
    assert_eq!(
        shared.dropout_last_reason.load(Ordering::Relaxed),
        DropoutReason::None as u8
    );
    // Deadline is published regardless.
    assert!(shared.callback_deadline_us.load(Ordering::Relaxed) > 10_000);
}

#[test]
fn medium_flags_block_over_eighty_percent_of_deadline() {
    let shared = SharedState::default();
    shared
        .dropout_protection_mode
        .store(AudioguardLevel::Normal as u8, Ordering::Relaxed);
    // 9_000 µs > 80% of 10_666 µs (≈ 8_533) → dropout under Medium.
    record_output_callback_timing(&shared, 9_000, FRAMES, SR, true);
    assert_eq!(shared.dropout_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        shared.dropout_last_reason.load(Ordering::Relaxed),
        DropoutReason::CallbackOverrun as u8
    );
}

#[test]
fn off_only_flags_true_overruns() {
    let shared = SharedState::default();
    shared
        .dropout_protection_mode
        .store(DropoutProtectionMode::Off as u8, Ordering::Relaxed);
    // Same 9_000 µs is < the full 10_666 µs deadline → no dropout under Off.
    record_output_callback_timing(&shared, 9_000, FRAMES, SR, true);
    assert_eq!(shared.dropout_count.load(Ordering::Relaxed), 0);
    // A genuine overrun past the deadline does flag, even under Off.
    record_output_callback_timing(&shared, 12_000, FRAMES, SR, true);
    assert_eq!(shared.dropout_count.load(Ordering::Relaxed), 1);
    // A block Audioguard rendered ahead is timed, never flagged: the ring,
    // not one period, is its headroom.
    record_output_callback_timing(&shared, 12_000, FRAMES, SR, false);
    assert_eq!(shared.dropout_count.load(Ordering::Relaxed), 1);
    // Only the block that really ran past its deadline in the callback is
    // an overrun; the near-miss at 9 ms and the block rendered ahead are not.
    assert_eq!(shared.callback_overruns.load(Ordering::Relaxed), 1);
}
