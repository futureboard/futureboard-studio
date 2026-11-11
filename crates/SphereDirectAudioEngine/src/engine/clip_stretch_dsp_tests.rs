use super::{clip_source_pos_seconds, sample_clip_processor_stereo, signalsmith_input_span};
use crate::audio_file::AudioFileBuffer;
use crate::audio_source::ClipAudioSource;
use crate::runtime::{resolve_clip_processor, ClipDspProcessor};
use std::sync::Arc;

/// Source sample index a given in-clip output offset reads from, with the
/// source/output sample rates matched at 48 kHz.
fn src_sample(rel: u64, duration: u64, speed: f32, reverse: bool) -> f64 {
    clip_source_pos_seconds(0.0, rel, duration, 48_000, speed, reverse) * 48_000.0
}

#[test]
fn manual_stretch_maps_output_to_source_samples() {
    // ratio 2.0 → speed 0.5; a 96k-output clip consumes the 48k source.
    assert!((src_sample(0, 96_000, 0.5, false) - 0.0).abs() < 1e-6);
    assert!((src_sample(24_000, 96_000, 0.5, false) - 12_000.0).abs() < 1e-6);
    assert!((src_sample(48_000, 96_000, 0.5, false) - 24_000.0).abs() < 1e-6);
    assert!((src_sample(95_999, 96_000, 0.5, false) - 47_999.5).abs() < 1e-3);
}

#[test]
fn resample_mode_changes_source_read_rate() {
    // speed 1.0 advances 1:1; 2.0 twice as fast; 0.5 half as fast.
    assert!((src_sample(100, 48_000, 1.0, false) - 100.0).abs() < 1e-6);
    assert!((src_sample(100, 48_000, 2.0, false) - 200.0).abs() < 1e-6);
    assert!((src_sample(100, 48_000, 0.5, false) - 50.0).abs() < 1e-6);
}

#[test]
fn reverse_mapping_reads_from_source_end_backwards() {
    // speed 1.0, 48k clip: output 0 → last source frame; last output → start.
    assert!((src_sample(0, 48_000, 1.0, true) - 47_999.0).abs() < 1e-6);
    assert!((src_sample(47_999, 48_000, 1.0, true) - 0.0).abs() < 1e-6);
    // Reverse reads strictly decreasing source positions.
    assert!(src_sample(0, 48_000, 1.0, true) > src_sample(1, 48_000, 1.0, true));
}

#[test]
fn processor_selection_routes_by_mode_and_preserve() {
    assert_eq!(
        resolve_clip_processor("off", false),
        ClipDspProcessor::NoStretch
    );
    assert_eq!(
        resolve_clip_processor("resample", true),
        ClipDspProcessor::Resample
    );
    assert_eq!(
        resolve_clip_processor("manual", false),
        ClipDspProcessor::Resample
    );
    assert_eq!(
        resolve_clip_processor("manual", true),
        ClipDspProcessor::PhaseVocoderBasic
    );
    assert_eq!(
        resolve_clip_processor("temposync", true),
        ClipDspProcessor::PhaseVocoderBasic
    );
    assert_eq!(
        resolve_clip_processor("warp", false),
        ClipDspProcessor::PhaseVocoderBasic
    );
}

#[test]
fn signalsmith_feed_tiles_source_without_gap_or_overlap() {
    // Across the whole clip, consecutive block input spans must tile the
    // source contiguously and total to floor(duration / ratio) — i.e. the
    // source length — so the stretcher never over-reads or backs up.
    for &(duration, ratio) in &[
        (96_000u64, 2.0_f64), // slow down 2× (source 48k)
        (24_000u64, 0.5_f64), // speed up 2× (source 48k)
        (50_000u64, 1.37_f64),
        (50_000u64, 0.73_f64),
    ] {
        let frames = 512usize;
        let mut rel = 0u64;
        let mut prev_end: Option<i64> = None;
        let mut total: i64 = 0;
        while rel < duration {
            let block = frames.min((duration - rel) as usize);
            let (in_start, input_frames) = signalsmith_input_span(rel, block, ratio);
            if let Some(end) = prev_end {
                assert_eq!(in_start, end, "blocks must tile with no gap/overlap");
            }
            prev_end = Some(in_start + input_frames as i64);
            total += input_frames as i64;
            rel += block as u64;
        }
        let expected = (duration as f64 / ratio).floor() as i64;
        // Allow the final partial block ±1 rounding, but never over-read.
        assert!(
            (total - expected).abs() <= 1,
            "consumed {total} source frames, expected ~{expected} (ratio {ratio})"
        );
    }
}

#[test]
fn preserve_pitch_processor_output_differs_from_resample() {
    let mut samples = Vec::new();
    for i in 0..4096 {
        let v = ((i as f32 * 0.017).sin() * (i as f32 * 0.003).cos()).clamp(-1.0, 1.0);
        samples.push(v);
        samples.push(v);
    }
    let source = ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 4096,
        samples,
    }));
    let resample =
        sample_clip_processor_stereo(&source, 512.0, 256.0, 2.0, ClipDspProcessor::Resample);
    let pv = sample_clip_processor_stereo(
        &source,
        512.0,
        256.0,
        2.0,
        ClipDspProcessor::PhaseVocoderBasic,
    );
    assert!(
        (resample.0 - pv.0).abs() > 1e-5 || (resample.1 - pv.1).abs() > 1e-5,
        "PhaseVocoderBasic should not be identical to Resample"
    );
}
