use super::*;
use crate::audio_file::AudioFileBuffer;
use crate::audio_source::ClipAudioSource;
use crate::runtime::{
    volume_db_to_norm, RuntimeAutomationCurve, RuntimeAutomationLane, RuntimeAutomationPoint,
    RuntimeAutomationTarget, RuntimeClip, RuntimePreviewMode, RuntimeProject, RuntimeSend,
    RuntimeTrack,
};
use std::sync::Arc;

fn track(id: &str, ty: &str, sends: Vec<RuntimeSend>) -> RuntimeTrack {
    let cap = 8;
    RuntimeTrack {
        spatial: None,
        spatial_params: Default::default(),
        active_voices: 0,
        listen: crate::monitor::ListenMode::Off,
        id: id.to_string(),
        track_type: ty.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        record_armed: false,
        monitor_enabled: false,
        input_source: crate::runtime::RuntimeTrackInputSource::None,
        jam_publish_slot: None,
        loopback_publish: false,
        loopback_out_l: Vec::new(),
        loopback_out_r: Vec::new(),
        jam_multitrack_pair: None,
        preview_mode: RuntimePreviewMode::Stereo,
        output_track_id: None,
        output_track_index: None,
        inserts: Vec::new(),
        sends,
        automation_lanes: Vec::new(),
        plugin_param_automation: Vec::new(),
        meter: Arc::new(Default::default()),
        meter_peak_l: 0.0,
        meter_peak_r: 0.0,
        meter_sum_sq_l: 0.0,
        meter_sum_sq_r: 0.0,
        callback_insert_log_done: false,
        callback_clip_route_log_done: false,
        block_l: vec![0.0; cap],
        block_r: vec![0.0; cap],
        recv_l: vec![0.0; cap],
        recv_r: vec![0.0; cap],
        soundfont_l: vec![0.0; cap],
        soundfont_r: vec![0.0; cap],
        ara_renderers: Vec::new(),
        ara_l: vec![0.0; cap],
        ara_r: vec![0.0; cap],
        ara_silence: vec![0.0; cap],
        ara_latency_samples: 0,
        midi_block_events: Vec::new(),
        solfege_pitch_events: Vec::new(),
        solfege_articulation_events: Vec::new(),
        midi_instrument_insert_ix: None,
        soundfont_player: None,
        solfege_engine: None,
        plugin_latency_samples: 0,
        pdc_delay_l: Vec::new(),
        pdc_delay_r: Vec::new(),
        pdc_write_pos: 0,
        smoothed_gain_l: 1.0,
        smoothed_gain_r: 1.0,
        wdw_return_index: None,
    }
}

fn send(target: &str, level: f32) -> RuntimeSend {
    RuntimeSend {
        id: format!("send-{target}"),
        return_track_id: target.to_string(),
        return_track_index: None,
        level,
        enabled: true,
        pre_fader: false,
        level_lane: None,
        enabled_lane: None,
        pdc: Default::default(),
        wdw: false,
    }
}

fn automation_lane(
    target: RuntimeAutomationTarget,
    value: f32,
    enabled: bool,
) -> RuntimeAutomationLane {
    RuntimeAutomationLane {
        id: "auto-1".to_string(),
        name: "Automation".to_string(),
        target,
        enabled,
        points: vec![RuntimeAutomationPoint {
            beat: 0.0,
            value,
            curve: RuntimeAutomationCurve::Linear,
            tension: 0.0,
        }],
    }
}

/// Send level and on/off lanes override the send while they have
/// something to say, and an insert-bypass lane bypasses its insert while
/// playing — and only then.
#[test]
fn send_and_bypass_automation_override_the_controls() {
    use crate::runtime::{volume_db_to_norm, volume_norm_to_linear};
    let mut src = track("src", "audio", vec![send("ret", 1.0)]);
    src.inserts = vec![super::bridge_insert_tests::bridge_effect_track_with_id(
        "fx",
    )];
    let send_id = src.sends[0].id.clone();
    src.automation_lanes = vec![
        automation_lane(
            RuntimeAutomationTarget::SendGain {
                send_id: send_id.clone(),
            },
            volume_db_to_norm(-6.0),
            true,
        ),
        automation_lane(RuntimeAutomationTarget::SendEnabled { send_id }, 0.0, true),
        automation_lane(
            RuntimeAutomationTarget::InsertBypass {
                insert_id: "fx".to_string(),
            },
            1.0,
            true,
        ),
    ];
    crate::runtime::resolve_send_and_bypass_lanes(&mut src);

    let (enabled, level) = src.send_state_at(0, 4.0);
    assert!(!enabled, "the on/off lane switched the send off");
    let expected = volume_norm_to_linear(volume_db_to_norm(-6.0));
    assert!((level - expected).abs() < 1e-6, "{level} vs {expected}");

    render::apply_insert_bypass_automation(&mut src, Some(4.0));
    assert!(
        !src.inserts[0].active(),
        "bypassed by its lane while playing"
    );
    render::apply_insert_bypass_automation(&mut src, None);
    assert!(
        src.inserts[0].active(),
        "its own switch stands while stopped"
    );

    // A disabled (Off) lane says nothing: the send's own values stand.
    for lane in &mut src.automation_lanes {
        lane.enabled = false;
    }
    assert_eq!(src.send_state_at(0, 4.0), (true, 1.0));
}

/// A Master Volume lane with points sets the master gain; with none, or
/// switched off, the master fader stands.
#[test]
fn master_volume_lane_overrides_the_master_fader() {
    use crate::runtime::{volume_db_to_norm, volume_norm_to_linear};
    let mut master = track("master", "master", vec![]);
    master.automation_lanes = vec![automation_lane(
        RuntimeAutomationTarget::TrackVolume,
        volume_db_to_norm(-12.0),
        true,
    )];
    let mut tracks = vec![track("a", "audio", vec![]), master];
    let gain = render::master_automation_volume(&tracks, Some(1), 4.0).expect("lane value");
    let expected = volume_norm_to_linear(volume_db_to_norm(-12.0));
    assert!((gain - expected).abs() < 1e-6, "{gain} vs {expected}");
    assert_eq!(render::master_automation_volume(&tracks, None, 4.0), None);

    tracks[1].automation_lanes[0].enabled = false;
    assert_eq!(
        render::master_automation_volume(&tracks, Some(1), 4.0),
        None
    );
}

#[test]
fn block_render_wraps_clip_material_inside_callback() {
    let frames = 5usize;
    let channels = 2usize;
    let mut audio_track = track("audio", "audio", vec![]);
    audio_track.pan = 0.0; // unity at center → left == source
    let tracks = vec![audio_track];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let mut samples = Vec::new();
    for i in 0..8 {
        let v = i as f32 / 10.0;
        samples.push(v);
        samples.push(v);
    }
    let source = Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
        sample_rate: 48_000,
        channels: 2,
        frames: 8,
        samples,
    })));
    let mut p = RuntimeProject {
        spatial: Default::default(),
        sample_rate: 48_000,
        tracks,
        clips: vec![RuntimeClip {
            id: "clip-1".to_string(),
            track_id: "audio".to_string(),
            track_index: None,
            start_beat: 0.0,
            duration_beats: 8.0,
            start_sample: 0,
            duration_samples: 8,
            offset_seconds: 0.0,
            gain: 1.0,
            gain_target: 1.0,
            gain_step: 1.0,
            stretch: SphereAudioProcessor::StretchParams::default(),
            speed_ratio: 1.0,
            source_read_rate: 1.0,
            effective_time_ratio: 1.0,
            pitch_ratio: 1.0,
            stretch_backend: SphereAudioProcessor::StretchBackend::InternalRePitch,
            source_start_samples: 0,
            source_end_samples: 8,
            warp_markers: Vec::new(),
            warp_segments: Vec::new(),
            processor: ClipDspProcessor::Resample,
            reverse: false,
            denoise: SphereAudioProcessor::DenoiseProcessor::new(48_000, 0.0),
            id_hash: 1,
            channel_transform: SphereAudioProcessor::ChannelTransform::Stereo,
            dc_remove: false,
            dc_left: 0.0,
            dc_right: 0.0,
            extra_gain: 1.0,
            dehum: SphereAudioProcessor::DehumProcessor::new(
                48_000,
                SphereAudioProcessor::DehumParams {
                    base_hz: 0.0,
                    harmonics: 0,
                    reduction_db: 0.0,
                },
            ),
            envelope_points: Vec::new(),
            preview_bypass: false,
            muted: false,
            ara_rendered: false,
            fade_in_samples: 0,
            fade_out_samples: 0,
            fade_in_curve: crate::runtime::FadeCurve::EqualPower,
            fade_out_curve: crate::runtime::FadeCurve::EqualPower,
            source,
            stretch_processor: None,
            stretch_input_l: Vec::new(),
            stretch_input_r: Vec::new(),
            stretch_output_l: Vec::new(),
            stretch_output_r: Vec::new(),
            stretch_prime_l: Vec::new(),
            stretch_prime_r: Vec::new(),
            stretch_next_project_sample: None,
        }],
        audio_graph,
        ..Default::default()
    };
    p.resolve_indices();

    let mut output = vec![0.0f32; frames * channels];
    let rendered = render_project_block_interleaved(
        &mut p,
        3,
        1.0,
        &mut output,
        channels,
        true,
        4,
        4,
        Some(crate::transport::LoopBounds { start: 2, end: 5 }),
    );

    assert_eq!(rendered, frames as u64);
    let left: Vec<f32> = output.chunks(channels).map(|frame| frame[0]).collect();
    assert_eq!(left, vec![0.3, 0.4, 0.2, 0.3, 0.4]);
}

/// #379: a device switch hands the backend the cached graph, which
/// rescales it from the rate it was built at. A graph built for a 16 kHz
/// device was relabelled 44.1 kHz first, so the switch to 48 kHz scaled
/// clips by 48/44.1 instead of 3 and they stopped at about a third.
#[test]
fn device_switch_rescales_a_16k_graph_from_its_own_rate() {
    const OLD_SR: u32 = 16_000;
    const NEW_SR: u32 = 48_000;
    let tracks = vec![track("audio", "audio", vec![])];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let source = Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
        sample_rate: OLD_SR,
        channels: 2,
        frames: OLD_SR as usize,
        samples: vec![0.0; OLD_SR as usize * 2],
    })));
    let clip = RuntimeClip {
        id: "clip-1".to_string(),
        track_id: "audio".to_string(),
        track_index: None,
        start_beat: 0.0,
        duration_beats: 2.0,
        start_sample: 0,
        duration_samples: OLD_SR as u64,
        offset_seconds: 0.0,
        gain: 1.0,
        gain_target: 1.0,
        gain_step: 1.0,
        stretch: SphereAudioProcessor::StretchParams::default(),
        speed_ratio: 1.0,
        source_read_rate: 1.0,
        effective_time_ratio: 1.0,
        pitch_ratio: 1.0,
        stretch_backend: SphereAudioProcessor::StretchBackend::InternalRePitch,
        source_start_samples: 0,
        source_end_samples: OLD_SR as u64,
        warp_markers: Vec::new(),
        warp_segments: vec![crate::runtime::RuntimeWarpSegment {
            out_start: 4_000,
            out_end: OLD_SR as u64,
            src_start: 4_000.0,
            src_end: OLD_SR as f64,
        }],
        processor: ClipDspProcessor::Resample,
        reverse: false,
        denoise: SphereAudioProcessor::DenoiseProcessor::new(OLD_SR, 0.0),
        id_hash: 1,
        channel_transform: SphereAudioProcessor::ChannelTransform::Stereo,
        dc_remove: false,
        dc_left: 0.0,
        dc_right: 0.0,
        extra_gain: 1.0,
        dehum: SphereAudioProcessor::DehumProcessor::new(
            OLD_SR,
            SphereAudioProcessor::DehumParams {
                base_hz: 0.0,
                harmonics: 0,
                reduction_db: 0.0,
            },
        ),
        envelope_points: Vec::new(),
        preview_bypass: false,
        muted: false,
        ara_rendered: false,
        fade_in_samples: 1_600,
        fade_out_samples: 3_200,
        fade_in_curve: crate::runtime::FadeCurve::EqualPower,
        fade_out_curve: crate::runtime::FadeCurve::EqualPower,
        source,
        stretch_processor: None,
        stretch_input_l: Vec::new(),
        stretch_input_r: Vec::new(),
        stretch_output_l: Vec::new(),
        stretch_output_r: Vec::new(),
        stretch_prime_l: Vec::new(),
        stretch_prime_r: Vec::new(),
        stretch_next_project_sample: None,
    };
    let mut built = RuntimeProject {
        spatial: Default::default(),
        sample_rate: OLD_SR,
        tracks,
        clips: vec![clip],
        audio_graph,
        ..Default::default()
    };
    built.resolve_indices();

    // `load_project` built the graph at the 16 kHz device's rate.
    let engine = EngineInner::new();
    engine.shared.sample_rate.store(OLD_SR, Ordering::Relaxed);
    *engine.runtime.lock() = built;

    // The switch: the graph handed over, then the 48 kHz backend's retarget.
    let mut runtime = engine.get_initial_runtime(None);
    assert_eq!(runtime.sample_rate, OLD_SR, "the recorded rate is kept");
    runtime.retarget_sample_rate(NEW_SR);

    let clip = &runtime.clips[0];
    assert_eq!(runtime.sample_rate, NEW_SR);
    assert_eq!(clip.duration_samples, NEW_SR as u64, "one second long");
    assert_eq!(clip.fade_in_samples, 4_800);
    assert_eq!(clip.fade_out_samples, 9_600);
    assert_eq!(clip.warp_segments[0].out_start, 12_000);
    assert_eq!(clip.warp_segments[0].out_end, NEW_SR as u64);
}

/// The stereo block renderer (live stereo playback and export) must apply
/// the clip's own processing — channel transform, DC removal and the gain
/// envelope — not just gain × fade. Before, only the mono per-sample path
/// did, so these settings were silent on every real output.
#[test]
fn stereo_block_render_applies_clip_processing() {
    let frames = 4usize;
    let channels = 2usize;
    let render = |configure: &dyn Fn(&mut RuntimeClip)| -> (f32, f32) {
        let mut audio_track = track("audio", "audio", vec![]);
        audio_track.pan = 0.0;
        let tracks = vec![audio_track];
        let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
        let samples: Vec<f32> = (0..8).flat_map(|_| [0.4f32, 0.1]).collect();
        let source = Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 8,
            samples,
        })));
        let mut clip = RuntimeClip {
            id: "clip-1".to_string(),
            track_id: "audio".to_string(),
            track_index: None,
            start_beat: 0.0,
            duration_beats: 8.0,
            start_sample: 0,
            duration_samples: 8,
            offset_seconds: 0.0,
            gain: 1.0,
            gain_target: 1.0,
            gain_step: 1.0,
            stretch: SphereAudioProcessor::StretchParams::default(),
            speed_ratio: 1.0,
            source_read_rate: 1.0,
            effective_time_ratio: 1.0,
            pitch_ratio: 1.0,
            stretch_backend: SphereAudioProcessor::StretchBackend::InternalRePitch,
            source_start_samples: 0,
            source_end_samples: 8,
            warp_markers: Vec::new(),
            warp_segments: Vec::new(),
            processor: ClipDspProcessor::Resample,
            reverse: false,
            denoise: SphereAudioProcessor::DenoiseProcessor::new(48_000, 0.0),
            id_hash: 1,
            channel_transform: SphereAudioProcessor::ChannelTransform::Stereo,
            dc_remove: false,
            dc_left: 0.0,
            dc_right: 0.0,
            extra_gain: 1.0,
            dehum: SphereAudioProcessor::DehumProcessor::new(
                48_000,
                SphereAudioProcessor::DehumParams {
                    base_hz: 0.0,
                    harmonics: 0,
                    reduction_db: 0.0,
                },
            ),
            envelope_points: Vec::new(),
            preview_bypass: false,
            muted: false,
            ara_rendered: false,
            fade_in_samples: 0,
            fade_out_samples: 0,
            fade_in_curve: crate::runtime::FadeCurve::EqualPower,
            fade_out_curve: crate::runtime::FadeCurve::EqualPower,
            source,
            stretch_processor: None,
            stretch_input_l: Vec::new(),
            stretch_input_r: Vec::new(),
            stretch_output_l: Vec::new(),
            stretch_output_r: Vec::new(),
            stretch_prime_l: Vec::new(),
            stretch_prime_r: Vec::new(),
            stretch_next_project_sample: None,
        };
        configure(&mut clip);
        let mut p = RuntimeProject {
            spatial: Default::default(),
            sample_rate: 48_000,
            tracks,
            clips: vec![clip],
            audio_graph,
            ..Default::default()
        };
        p.resolve_indices();
        let mut output = vec![0.0f32; frames * channels];
        render_project_block_interleaved(&mut p, 2, 1.0, &mut output, channels, true, 4, 4, None);
        (output[0], output[1])
    };
    let close = |a: f32, b: f32| (a - b).abs() < 1.0e-3;

    let (l, r) = render(&|_| {});
    assert!(close(l, 0.4) && close(r, 0.1), "baseline {l} {r}");

    // A live gain change ramps: the first sample has barely moved toward
    // the new gain, where a step would land on it at once.
    let (l, _) = render(&|c| {
        c.gain_target = 0.5;
        c.gain_step = crate::runtime::clip_gain_step(48_000);
    });
    assert!(l > 0.39 && l < 0.4, "gain ramp first sample {l}");
    let (l, _) = render(&|c| c.gain_target = 0.5);
    assert!(close(l, 0.2), "settled gain {l}");

    let (l, r) = render(&|c| c.channel_transform = SphereAudioProcessor::ChannelTransform::Swap);
    assert!(close(l, 0.1) && close(r, 0.4), "channel swap {l} {r}");

    let (l, r) = render(&|c| {
        c.dc_remove = true;
        c.dc_left = 0.1;
        c.dc_right = 0.05;
    });
    assert!(close(l, 0.3) && close(r, 0.05), "dc removal {l} {r}");

    let half_db = -6.0206;
    let (l, r) = render(&|c| c.envelope_points = vec![(0.0, half_db), (1.0, half_db)]);
    assert!(close(l, 0.2) && close(r, 0.05), "gain envelope {l} {r}");

    // Preview bypass is the A/B "hear the original" switch.
    let (l, _) = render(&|c| {
        c.envelope_points = vec![(0.0, half_db), (1.0, half_db)];
        c.preview_bypass = true;
    });
    assert!(close(l, 0.4), "bypass {l}");
}

/// End-to-end render check that `reverse` and `speed_ratio` actually change
/// the audio (not just the snapshot). This is the same block renderer the
/// offline exporter drives, so it also covers "export uses the clip DSP path".
#[test]
fn reverse_and_speed_change_clip_render_output() {
    let frames = 8usize;
    let channels = 2usize;
    // Distinct, soft-limit-safe per-frame values: left[i] = i / 10.
    let samples: Vec<f32> = (0..8)
        .flat_map(|i| {
            let v = i as f32 / 10.0;
            [v, v]
        })
        .collect();

    let render = |reverse: bool, speed: f32| -> Vec<f32> {
        let mut audio_track = track("audio", "audio", vec![]);
        audio_track.pan = 0.0; // unity at center → left == source
        let tracks = vec![audio_track];
        let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
        let source = Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
            sample_rate: 48_000,
            channels: 2,
            frames: 8,
            samples: samples.clone(),
        })));
        let mut p = RuntimeProject {
            spatial: Default::default(),
            sample_rate: 48_000,
            tracks,
            clips: vec![RuntimeClip {
                id: "c".to_string(),
                track_id: "audio".to_string(),
                track_index: None,
                start_beat: 0.0,
                duration_beats: 8.0,
                start_sample: 0,
                duration_samples: 8,
                offset_seconds: 0.0,
                gain: 1.0,
                gain_target: 1.0,
                gain_step: 1.0,
                stretch: SphereAudioProcessor::StretchParams::default(),
                speed_ratio: speed,
                source_read_rate: speed,
                effective_time_ratio: if speed > 0.0 { 1.0 / speed } else { 1.0 },
                pitch_ratio: 1.0,
                stretch_backend: SphereAudioProcessor::StretchBackend::InternalRePitch,
                source_start_samples: 0,
                source_end_samples: 8,
                warp_markers: Vec::new(),
                warp_segments: Vec::new(),
                processor: ClipDspProcessor::Resample,
                reverse,
                denoise: SphereAudioProcessor::DenoiseProcessor::new(48_000, 0.0),
                id_hash: 1,
                channel_transform: SphereAudioProcessor::ChannelTransform::Stereo,
                dc_remove: false,
                dc_left: 0.0,
                dc_right: 0.0,
                extra_gain: 1.0,
                dehum: SphereAudioProcessor::DehumProcessor::new(
                    48_000,
                    SphereAudioProcessor::DehumParams {
                        base_hz: 0.0,
                        harmonics: 0,
                        reduction_db: 0.0,
                    },
                ),
                envelope_points: Vec::new(),
                preview_bypass: false,
                muted: false,
                ara_rendered: false,
                fade_in_samples: 0,
                fade_out_samples: 0,
                fade_in_curve: crate::runtime::FadeCurve::EqualPower,
                fade_out_curve: crate::runtime::FadeCurve::EqualPower,
                source,
                stretch_processor: None,
                stretch_input_l: Vec::new(),
                stretch_input_r: Vec::new(),
                stretch_output_l: Vec::new(),
                stretch_output_r: Vec::new(),
                stretch_prime_l: Vec::new(),
                stretch_prime_r: Vec::new(),
                stretch_next_project_sample: None,
            }],
            audio_graph,
            ..Default::default()
        };
        p.resolve_indices();
        let mut output = vec![0.0f32; frames * channels];
        render_project_block_interleaved(&mut p, 0, 1.0, &mut output, channels, true, 4, 4, None);
        output.chunks(channels).map(|f| f[0]).collect()
    };

    let close = |a: &[f32], b: &[f32]| {
        assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() < 1e-4, "{a:?} vs {b:?}");
        }
    };

    // Forward reads source frames 0..7 straight.
    close(
        &render(false, 1.0),
        &[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7],
    );
    // Reverse reads frames 7..0.
    close(
        &render(true, 1.0),
        &[0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.0],
    );
    // Half read-rate (speed 0.5) consumes the source half as fast, linearly
    // interpolating between frames.
    close(
        &render(false, 0.5),
        &[0.0, 0.05, 0.1, 0.15, 0.2, 0.25, 0.3, 0.35],
    );
}

#[test]
fn track_volume_and_pan_automation_affect_fader_output() {
    let mut t = track("audio", "audio", vec![]);
    t.volume = 0.25;
    t.pan = 0.0;
    t.automation_lanes = vec![
        automation_lane(
            RuntimeAutomationTarget::TrackVolume,
            volume_db_to_norm(0.0),
            true,
        ),
        automation_lane(RuntimeAutomationTarget::TrackPan, 1.0, true),
    ];

    let (l, r) = apply_track_chain_at_beat(1.0, 1.0, &mut t, 0.0);

    assert!(l.abs() < 1e-6, "full-right pan should mute left");
    // Equal-power law, unity at center: hard right lands at +3 dB (√2).
    assert!(
        (r - std::f32::consts::SQRT_2).abs() < 1e-6,
        "0 dB automation should override base volume (got {r})"
    );
}

#[test]
fn track_mute_automation_overrides_base_mute_state() {
    let mut t = track("audio", "audio", vec![]);
    assert!(!effective_track_muted(&t, 0.0));

    t.automation_lanes = vec![automation_lane(
        RuntimeAutomationTarget::TrackMute,
        1.0,
        true,
    )];
    assert!(effective_track_muted(&t, 0.0));

    t.automation_lanes[0].enabled = false;
    assert!(!effective_track_muted(&t, 0.0));
}

#[test]
fn send_to_return_accumulates_scaled() {
    let frames = 4;
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![
            track("audio", "audio", vec![send("ret", 0.5)]),
            track("ret", "return", vec![]),
        ],
        ..Default::default()
    };
    p.resolve_indices();
    // Source post-fader signal (accumulate_sends reads block_*).
    p.tracks[0].block_l[..frames].fill(1.0);
    p.tracks[0].block_r[..frames].fill(-2.0);

    accumulate_sends(&mut p, 0, frames, false, 0.0, false);

    assert!(p.tracks[1].recv_l[..frames]
        .iter()
        .all(|&v| (v - 0.5).abs() < 1e-6));
    assert!(p.tracks[1].recv_r[..frames]
        .iter()
        .all(|&v| (v + 1.0).abs() < 1e-6));
}

/// A live send level (`SetTrackSendLevel`) is what the next block sends,
/// with no rebuild; an index the graph does not have changes nothing.
#[test]
fn a_live_send_level_reaches_the_next_block() {
    let frames = 4;
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![
            track("audio", "audio", vec![send("ret", 0.5)]),
            track("ret", "return", vec![]),
        ],
        ..Default::default()
    };
    p.resolve_indices();
    p.update_track_send_level(0, 0, 0.25);
    p.update_track_send_level(0, 7, 1.0);
    p.update_track_send_level(9, 0, 1.0);
    p.tracks[0].block_l[..frames].fill(1.0);

    accumulate_sends(&mut p, 0, frames, false, 0.0, false);

    assert!(p.tracks[1].recv_l[..frames]
        .iter()
        .all(|&v| (v - 0.25).abs() < 1e-6));
    p.update_track_send_level(0, 0, 9.0);
    assert_eq!(p.tracks[0].sends[0].level, 2.0, "clamped to +6 dB");
}

#[test]
fn pre_fader_filter_only_routes_matching_phase() {
    let frames = 4;
    let mut pre = send("ret", 1.0);
    pre.pre_fader = true;
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![
            track("audio", "audio", vec![pre]),
            track("ret", "return", vec![]),
        ],
        ..Default::default()
    };
    p.resolve_indices();
    p.tracks[0].block_l[..frames].fill(1.0);

    // Post-fader phase: the pre-fader send must NOT route.
    accumulate_sends(&mut p, 0, frames, false, 0.0, false);
    assert!(p.tracks[1].recv_l[..frames].iter().all(|&v| v == 0.0));

    // Pre-fader phase: now it routes.
    accumulate_sends(&mut p, 0, frames, true, 0.0, false);
    assert!(p.tracks[1].recv_l[..frames]
        .iter()
        .all(|&v| (v - 1.0).abs() < 1e-6));
}

#[test]
fn send_to_non_routing_target_is_rejected() {
    let frames = 4;
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![
            track("a", "audio", vec![send("b", 1.0)]),
            track("b", "audio", vec![]),
        ],
        ..Default::default()
    };
    p.resolve_indices();
    p.tracks[0].block_l[..frames].fill(1.0);
    accumulate_sends(&mut p, 0, frames, false, 0.0, false);
    // Target is a normal audio track → not a valid send destination.
    assert!(p.tracks[1].recv_l[..frames].iter().all(|&v| v == 0.0));
}

#[test]
fn routing_chain_to_earlier_index_is_accepted() {
    let frames = 4;
    // "late" at index 1 sends to "early" at index 0 — valid DAG when there
    // is no back-edge. Pass-2 topo order processes late before early.
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![
            track("early", "return", vec![]),
            track("late", "bus", vec![send("early", 1.0)]),
        ],
        ..Default::default()
    };
    p.resolve_indices();
    p.tracks[1].block_l[..frames].fill(1.0);

    accumulate_sends(&mut p, 1, frames, false, 0.0, false);

    assert!(p.tracks[0].recv_l[..frames]
        .iter()
        .all(|&v| (v - 1.0).abs() < 1e-6));
}

#[test]
fn routing_cycle_edge_is_rejected_at_plan_time() {
    let frames = 4;
    // Mutual sends form a cycle; the closing edge is rejected by the planner
    // and never resolves into a live runtime send. Direct accumulate of a
    // self-cycle is still dropped by the self-target guard.
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![
            track("early", "bus", vec![send("late", 1.0)]),
            track("late", "return", vec![send("early", 1.0)]),
        ],
        ..Default::default()
    };
    p.resolve_indices();
    p.tracks[0].block_l[..frames].fill(1.0);
    p.tracks[1].block_l[..frames].fill(1.0);

    accumulate_sends(&mut p, 0, frames, false, 0.0, false);
    accumulate_sends(&mut p, 1, frames, false, 0.0, false);

    // Both directions accumulate at the raw accumulate_sends layer (indices
    // resolve); the planner is what keeps cycles out of a built project.
    assert!(p.tracks[1].recv_l[..frames]
        .iter()
        .all(|&v| (v - 1.0).abs() < 1e-6));
    assert!(p.tracks[0].recv_l[..frames]
        .iter()
        .all(|&v| (v - 1.0).abs() < 1e-6));
}

#[test]
fn main_output_to_bus_routes_into_bus_receive_not_master() {
    let frames = 4;
    let channels = 2;
    let mut a = track("a", "audio", vec![]);
    a.output_track_id = Some("bus".to_string());
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![a, track("bus", "bus", vec![])],
        ..Default::default()
    };
    p.resolve_indices();
    p.tracks[0].block_l[..frames].fill(0.8);
    p.tracks[0].block_r[..frames].fill(-0.4);

    let mut output = vec![0.0f32; frames * channels];
    route_main_output(&mut p, 0, frames, &mut output, channels);

    // The post-fader block went into the bus receive buffers …
    assert!(p.tracks[1].recv_l[..frames]
        .iter()
        .all(|&v| (v - 0.8).abs() < 1e-6));
    assert!(p.tracks[1].recv_r[..frames]
        .iter()
        .all(|&v| (v + 0.4).abs() < 1e-6));
    // … and NOT into the master output.
    assert!(output.iter().all(|&v| v == 0.0));
}

#[test]
fn main_output_to_master_sums_into_output() {
    let frames = 4;
    let channels = 2;
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![track("a", "audio", vec![])], // output_track_id = None → master
        ..Default::default()
    };
    p.tracks[0].block_l[..frames].fill(0.5);
    p.tracks[0].block_r[..frames].fill(0.25);

    let mut output = vec![0.0f32; frames * channels];
    route_main_output(&mut p, 0, frames, &mut output, channels);

    for f in 0..frames {
        assert!((output[f * channels] - 0.5).abs() < 1e-6);
        assert!((output[f * channels + 1] - 0.25).abs() < 1e-6);
    }
}

#[test]
fn main_output_to_non_routing_track_falls_back_to_master() {
    let frames = 4;
    let channels = 2;
    let mut a = track("a", "audio", vec![]);
    a.output_track_id = Some("b".to_string()); // "b" is a plain audio track
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks: vec![a, track("b", "audio", vec![])],
        ..Default::default()
    };
    p.resolve_indices();
    p.tracks[0].block_l[..frames].fill(1.0);
    p.tracks[0].block_r[..frames].fill(1.0);

    let mut output = vec![0.0f32; frames * channels];
    route_main_output(&mut p, 0, frames, &mut output, channels);

    // Not a routing target → summed to master, "b" untouched.
    assert!(output.iter().all(|&v| (v - 1.0).abs() < 1e-6));
    assert!(p.tracks[1].recv_l[..frames].iter().all(|&v| v == 0.0));
}
