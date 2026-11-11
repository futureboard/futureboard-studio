use super::{log_sample_rate_decision, sample_rate_mismatch, sample_rate_mode_label};
use crate::backend::BackendKind;

#[test]
fn shared_mode_requested_48k_active_96k_is_a_mismatch() {
    // The exact reported scenario: Preferences requested 48000, but the
    // device opened at 96000 (WASAPI shared / device default).
    assert_eq!(sample_rate_mismatch(48_000, 96_000), Some((48_000, 96_000)));
}

#[test]
fn matching_rates_report_no_mismatch() {
    assert_eq!(sample_rate_mismatch(48_000, 48_000), None);
    assert_eq!(sample_rate_mismatch(96_000, 96_000), None);
}

#[test]
fn zero_requested_means_device_default_no_mismatch() {
    // requested == 0 ("use device default") is never a mismatch, whatever
    // the device opened at.
    assert_eq!(sample_rate_mismatch(0, 96_000), None);
    assert_eq!(sample_rate_mismatch(48_000, 0), None);
}

#[test]
fn shared_mismatch_is_visible_but_not_an_error() {
    // Shared/Auto mismatch is allowed (Windows resamples) — logged, but no
    // error string is returned for the UI's error channel.
    assert_eq!(
        log_sample_rate_decision(48_000, 96_000, &BackendKind::WasapiShared),
        None
    );
    assert_eq!(
        log_sample_rate_decision(48_000, 96_000, &BackendKind::Auto),
        None
    );
}

#[test]
fn exclusive_mismatch_returns_a_visible_warning() {
    // Exclusive mode could not honor the rate and fell back — surfaced as a
    // warning string the caller stores in last_daux_error.
    let warning = log_sample_rate_decision(48_000, 96_000, &BackendKind::WasapiExclusive)
        .expect("exclusive fallback must surface a warning");
    assert!(warning.contains("48000"));
    assert!(warning.contains("96000"));
}

#[test]
fn exclusive_without_mismatch_has_no_warning() {
    assert_eq!(
        log_sample_rate_decision(96_000, 96_000, &BackendKind::WasapiExclusive),
        None
    );
}

#[test]
fn mode_labels_are_stable() {
    assert_eq!(
        sample_rate_mode_label(&BackendKind::WasapiShared),
        "WASAPI_SHARED"
    );
    assert_eq!(
        sample_rate_mode_label(&BackendKind::WasapiExclusive),
        "WASAPI_EXCLUSIVE"
    );
}
