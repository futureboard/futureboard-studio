use super::*;
use crate::plugin_bridge::PluginBridgeSink;
use crate::runtime::{InsertDspState, RuntimeInsert, RuntimePreviewMode, RuntimeTrack};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct WetEffectSink {
    done_seq: AtomicU64,
    requests: AtomicU64,
    latency_samples: u32,
}

impl PluginBridgeSink for WetEffectSink {
    fn dsp_ready(&self) -> bool {
        true
    }

    fn read_output(&self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) -> usize {
        if self.done_seq.load(Ordering::Acquire) == 0 {
            return 0;
        }
        for i in 0..frames.min(out_l.len()).min(out_r.len()) {
            out_l[i] = 0.25;
            out_r[i] = 0.25;
        }
        frames
    }

    fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}

    fn write_input(&self, _: &[f32], _: &[f32], _: usize) {}

    fn request_block(&self, _: u32) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.done_seq.store(1, Ordering::Release);
    }

    fn reported_latency_samples(&self) -> u32 {
        self.latency_samples
    }
}

fn bridge_effect_track(block: f32) -> RuntimeTrack {
    let mut params = HashMap::new();
    params.insert("role".to_string(), serde_json::json!("effect"));
    RuntimeTrack {
        spatial: None,
        spatial_params: Default::default(),
        active_voices: 0,
        listen: crate::monitor::ListenMode::Off,
        id: "track-1".to_string(),
        track_type: "audio".to_string(),
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
        inserts: vec![RuntimeInsert {
            id: "insert-1".to_string(),
            kind: "external-bridge-plugin".to_string(),
            kind_tag: crate::runtime::RuntimeInsertKind::ExternalBridge,
            enabled: true,
            bypass_lane: None,
            automation_bypassed: false,
            params: Arc::new(params),
            bridge_is_effect: true,
            bridge_is_builtin: false,
            bridge_enabled_output_channels: Vec::new(),
            bridge_sink: None,
            dsp: InsertDspState::default(),
            vst3: None,
            cpu_us: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
            guard: Default::default(),
            callback_process_log_done: false,
            silent_process_blocks: 0,
            bridge_missed_blocks: 0,
            scratch_l: vec![0.0; 8],
            scratch_r: vec![0.0; 8],
            vsti_output_children: Vec::new(),
            scratch_multi: Vec::new(),
            chain_latency_samples: 0,
            signal_bus: 0,
        }],
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        plugin_param_automation: Vec::new(),
        meter: Arc::new(Default::default()),
        meter_peak_l: 0.0,
        meter_peak_r: 0.0,
        meter_sum_sq_l: 0.0,
        meter_sum_sq_r: 0.0,
        callback_insert_log_done: false,
        callback_clip_route_log_done: false,
        block_l: vec![block; 8],
        block_r: vec![block; 8],
        recv_l: vec![0.0; 8],
        recv_r: vec![0.0; 8],
        soundfont_l: vec![0.0; 8],
        soundfont_r: vec![0.0; 8],
        ara_renderers: Vec::new(),
        ara_l: vec![0.0; 8],
        ara_r: vec![0.0; 8],
        ara_silence: vec![0.0; 8],
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

#[test]
fn bridged_vst3_and_builtin_effects_process_on_master_when_sink_is_bound() {
    for bridge_is_builtin in [false, true] {
        let mut track = bridge_effect_track(1.0);
        track.track_type = "master".to_string();
        track.inserts[0].bridge_is_builtin = bridge_is_builtin;
        let sink = WetEffectSink {
            done_seq: AtomicU64::new(1),
            requests: AtomicU64::new(0),
            latency_samples: 0,
        };
        track.inserts[0].bridge_sink = Some(std::sync::Arc::new(sink));
        apply_track_chain_block(&mut track, 4, RuntimeTransportContext::default());
        assert!(
            (track.block_l[0] - 0.25).abs() < 1e-6,
            "master bridge format={} left={}",
            if bridge_is_builtin { "BuiltIn" } else { "VST3" },
            track.block_l[0]
        );
        assert!(
            (track.block_r[0] - 0.25).abs() < 1e-6,
            "master bridge format={} right={}",
            if bridge_is_builtin { "BuiltIn" } else { "VST3" },
            track.block_r[0]
        );
    }
}

#[test]
fn missed_bridge_effect_does_not_overwrite_an_in_flight_request() {
    #[derive(Debug, Default)]
    struct InFlightSink {
        writes: AtomicU64,
        requests: AtomicU64,
    }

    impl PluginBridgeSink for InFlightSink {
        fn dsp_ready(&self) -> bool {
            true
        }
        fn request_in_flight(&self) -> bool {
            true
        }
        fn read_output(&self, _: &mut [f32], _: &mut [f32], _: usize) -> usize {
            0
        }
        fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}
        fn write_input(&self, _: &[f32], _: &[f32], _: usize) {
            self.writes.fetch_add(1, Ordering::Relaxed);
        }
        fn request_block(&self, _: u32) {
            self.requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut track = bridge_effect_track(0.75);
    let sink = Arc::new(InFlightSink::default());
    track.inserts[0].bridge_sink = Some(sink.clone());

    apply_track_chain_block(&mut track, 4, RuntimeTransportContext::default());

    assert_eq!(track.block_l[0], 0.75, "a missed effect must bypass dry");
    assert_eq!(sink.writes.load(Ordering::Relaxed), 0);
    assert_eq!(sink.requests.load(Ordering::Relaxed), 0);
}

#[derive(Debug, Default)]
struct ParamCaptureSink {
    last_param_id: AtomicU64,
    last_value_bits: AtomicU64,
    pushes: AtomicU64,
}

impl PluginBridgeSink for ParamCaptureSink {
    fn dsp_ready(&self) -> bool {
        true
    }
    fn read_output(&self, _l: &mut [f32], _r: &mut [f32], _frames: usize) -> usize {
        0
    }
    fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}
    fn push_param(&self, param_id: u32, value: f32, _sample_offset: u32) {
        self.last_param_id.store(param_id as u64, Ordering::Release);
        self.last_value_bits
            .store(value.to_bits() as u64, Ordering::Release);
        self.pushes.fetch_add(1, Ordering::Release);
    }
    fn write_input(&self, _: &[f32], _: &[f32], _: usize) {}
    fn request_block(&self, _: u32) {}
}

fn plugin_param_lane(insert_id: &str, param_id: &str) -> crate::runtime::RuntimeAutomationLane {
    crate::runtime::RuntimeAutomationLane {
        id: "lane-1".to_string(),
        name: "Cutoff".to_string(),
        target: crate::runtime::RuntimeAutomationTarget::PluginParameter {
            insert_id: insert_id.to_string(),
            parameter_id: param_id.to_string(),
        },
        enabled: true,
        points: vec![
            crate::runtime::RuntimeAutomationPoint {
                beat: 0.0,
                value: 0.0,
                curve: crate::runtime::RuntimeAutomationCurve::Linear,
                tension: 0.0,
            },
            crate::runtime::RuntimeAutomationPoint {
                beat: 4.0,
                value: 1.0,
                curve: crate::runtime::RuntimeAutomationCurve::Linear,
                tension: 0.0,
            },
        ],
    }
}

#[test]
fn plugin_param_automation_pushes_value_to_bridge_sink_during_playback() {
    let mut track = bridge_effect_track(0.0);
    // Bridge plugin param automation: lane targets insert-1 / param 42.
    track.automation_lanes = vec![plugin_param_lane("insert-1", "42")];
    track.plugin_param_automation = vec![crate::runtime::RuntimePluginParamBinding {
        insert_ix: 0,
        lane_ix: 0,
        param_id: 42,
        last_value: f32::NAN,
    }];
    let sink = std::sync::Arc::new(ParamCaptureSink::default());
    track.inserts[0].bridge_sink = Some(sink.clone());

    // Block at beat 2.0 → linear midpoint between 0.0 and 1.0 = 0.5.
    let transport = RuntimeTransportContext {
        playing: true,
        ppq_position: 2.0,
        ..RuntimeTransportContext::default()
    };
    apply_track_chain_block(&mut track, 4, transport);

    assert_eq!(sink.pushes.load(Ordering::Acquire), 1);
    assert_eq!(sink.last_param_id.load(Ordering::Acquire), 42);
    let pushed = f32::from_bits(sink.last_value_bits.load(Ordering::Acquire) as u32);
    assert!((pushed - 0.5).abs() < 1e-4, "pushed={pushed}");
}

#[test]
fn plugin_param_automation_dedupes_unchanged_value_and_is_idle_when_stopped() {
    let mut track = bridge_effect_track(0.0);
    track.automation_lanes = vec![plugin_param_lane("insert-1", "42")];
    track.plugin_param_automation = vec![crate::runtime::RuntimePluginParamBinding {
        insert_ix: 0,
        lane_ix: 0,
        param_id: 42,
        last_value: f32::NAN,
    }];
    let sink = std::sync::Arc::new(ParamCaptureSink::default());
    track.inserts[0].bridge_sink = Some(sink.clone());

    let transport = RuntimeTransportContext {
        playing: true,
        ppq_position: 2.0,
        ..RuntimeTransportContext::default()
    };
    // Same beat twice → second block must not re-push the identical value.
    apply_track_chain_block(&mut track, 4, transport);
    apply_track_chain_block(&mut track, 4, transport);
    assert_eq!(sink.pushes.load(Ordering::Acquire), 1);

    // Stopped transport must not drive parameter automation at all.
    let stopped = RuntimeTransportContext {
        playing: false,
        ppq_position: 3.5,
        ..RuntimeTransportContext::default()
    };
    apply_track_chain_block(&mut track, 4, stopped);
    assert_eq!(sink.pushes.load(Ordering::Acquire), 1);
}

#[test]
fn bridge_latency_refresh_updates_pdc_delays() {
    let frames = 16usize;
    let mut fast = bridge_effect_track(1.0);
    fast.id = "fast".to_string();
    fast.inserts.clear();
    let mut slow = bridge_effect_track(1.0);
    slow.id = "slow".to_string();
    slow.inserts[0].id = "slow-insert".to_string();
    let tracks = vec![fast, slow];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks,
        audio_graph,
        pdc_enabled: true,
        ..Default::default()
    };
    let sink = WetEffectSink {
        done_seq: AtomicU64::new(1),
        requests: AtomicU64::new(0),
        latency_samples: 16,
    };
    p.plugin_bridge_sinks
        .insert("slow-insert".to_string(), Arc::new(sink));
    // The block path reads the cached per-insert sink, exactly like the
    // SetPluginBridgeSink command handler.
    p.resolve_bridge_sinks();
    // `build` plans and reserves before the callback ever runs; a
    // hand-assembled project has to do the same or the realtime refresh has
    // no sized plan to rewrite.
    p.plan_latency_and_pdc();

    assert!(p.refresh_runtime_latency_graph(frames as u32));
    assert_eq!(p.latency_graph.track_plugin_latency[0], 0);
    assert_eq!(p.latency_graph.track_plugin_latency[1], 32);
    assert_eq!(p.latency_graph.track_pdc_delay[0], 32);
    assert_eq!(p.latency_graph.track_pdc_delay[1], 0);
    assert!(!p.refresh_runtime_latency_graph(frames as u32));

    // A take recording with compensation bypassed holds nothing back,
    // keeps it off while playback runs on after the take, and puts it back
    // once the transport stops.
    let shared = SharedState::default();
    let mut local = crate::backend::render::LocalAudioState::new(48_000.0);
    shared.record_pdc_bypass.store(true, Ordering::Relaxed);
    local.follow_record_pdc_bypass(&mut p, &shared, false);
    assert!(!p.pdc_enabled);
    assert_eq!(p.latency_graph.track_pdc_delay[0], 0);
    assert_eq!(
        crate::backend::render::metronome_compensation_delay_samples(&p),
        0
    );
    // Bridge latency refreshes during the take keep it bypassed.
    p.latency_graph.track_plugin_latency[1] = 0;
    assert!(p.refresh_runtime_latency_graph(frames as u32));
    assert_eq!(p.latency_graph.track_pdc_delay[0], 0);

    shared.record_pdc_bypass.store(false, Ordering::Relaxed);
    local.follow_record_pdc_bypass(&mut p, &shared, true);
    assert!(!p.pdc_enabled, "compensation came back mid-play");
    local.follow_record_pdc_bypass(&mut p, &shared, false);
    assert!(p.pdc_enabled);
    assert_eq!(p.latency_graph.track_pdc_delay[0], 32);

    // A graph swapped in during the take was planned with compensation.
    shared.record_pdc_bypass.store(true, Ordering::Relaxed);
    local.follow_record_pdc_bypass(&mut p, &shared, true);
    assert!(!p.pdc_enabled);
    p.set_pdc_enabled_realtime(true);
    local.follow_record_pdc_bypass(&mut p, &shared, true);
    assert!(!p.pdc_enabled);
    assert_eq!(p.latency_graph.track_pdc_delay[0], 0);
}

/// A bridged plugin whose reported latency moves while the graph stays put —
/// what a network plugin tracking its own jitter buffer looks like to the
/// host, as opposed to a fixed-latency effect.
#[derive(Debug, Default)]
struct DriftingLatencySink {
    latency_samples: AtomicU32,
}

impl PluginBridgeSink for DriftingLatencySink {
    fn dsp_ready(&self) -> bool {
        true
    }

    fn read_output(&self, _: &mut [f32], _: &mut [f32], frames: usize) -> usize {
        frames
    }

    fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}

    fn write_input(&self, _: &[f32], _: &[f32], _: usize) {}

    fn request_block(&self, _: u32) {}

    fn reported_latency_samples(&self) -> u32 {
        self.latency_samples.load(Ordering::Relaxed)
    }
}

#[test]
fn drifting_bridge_latency_refresh_matches_a_control_thread_replan() {
    // The realtime refresh rewrites the plan in place instead of rebuilding
    // it, so it has to land exactly where a fresh control-thread plan would
    // — including on the way back down, where a stale value left in a reused
    // vector would survive as phantom compensation.
    let frames = 64u32;
    let mut src = bridge_effect_track(1.0);
    src.id = "src".to_string();
    src.inserts[0].id = "src-insert".to_string();
    src.sends = vec![crate::runtime::RuntimeSend {
        id: "send-1".to_string(),
        return_track_id: "ret".to_string(),
        return_track_index: None,
        level: 1.0,
        enabled: true,
        pre_fader: false,
        level_lane: None,
        enabled_lane: None,
        pdc: Default::default(),
        wdw: false,
    }];
    let mut ret = bridge_effect_track(1.0);
    ret.id = "ret".to_string();
    ret.track_type = "return".to_string();
    ret.inserts.clear();
    let mut master = bridge_effect_track(1.0);
    master.id = "master".to_string();
    master.track_type = "master".to_string();
    master.inserts.clear();

    let tracks = vec![src, ret, master];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks,
        audio_graph,
        pdc_enabled: true,
        ..Default::default()
    };
    let sink = Arc::new(DriftingLatencySink::default());
    p.plugin_bridge_sinks
        .insert("src-insert".to_string(), sink.clone());
    p.resolve_bridge_sinks();
    p.plan_latency_and_pdc();

    // Up, further up, then back down — the drop is the case a rebuild used
    // to get for free.
    for latency in [64u32, 4096, 128] {
        sink.latency_samples.store(latency, Ordering::Relaxed);
        assert!(
            p.refresh_runtime_latency_graph(frames),
            "latency {latency} should be observed as a change"
        );

        let observed = p.latency_graph.clone();
        let expected = crate::latency_graph::plan_runtime_latency_graph(
            &mut p.tracks,
            &p.audio_graph,
            p.pdc_enabled,
        );
        assert_eq!(
            observed.track_plugin_latency, expected.track_plugin_latency,
            "strip latency at {latency}"
        );
        assert_eq!(
            observed.track_output_latency, expected.track_output_latency,
            "output latency at {latency}"
        );
        assert_eq!(
            observed.track_pdc_delay, expected.track_pdc_delay,
            "pdc delays at {latency}"
        );
        assert_eq!(
            observed.max_path_latency_samples, expected.max_path_latency_samples,
            "max path at {latency}"
        );

        // Refitting inside the reserved capacity has to leave every ring
        // long enough to actually serve its delay — `apply_pdc_delay_block`
        // bypasses a track whose ring is too short.
        for (idx, track) in p.tracks.iter().enumerate() {
            let delay = observed.track_pdc_delay[idx] as usize;
            assert!(
                delay == 0 || track.pdc_delay_l.len() > delay,
                "track {idx} ring {} cannot serve delay {delay} at latency {latency}",
                track.pdc_delay_l.len()
            );
        }

        assert!(
            !p.refresh_runtime_latency_graph(frames),
            "a settled latency must not keep re-planning"
        );
    }
}

/// #337: a plug-in on the wet/dry/wet bus runs on the return, so its
/// latency is the wet path's. Planned on the source strip, the dry path
/// went uncompensated (early) while the plan believed it late.
#[test]
fn wdw_bus_latency_is_compensated_on_the_wet_return() {
    let frames = 64u32;
    let build = |send_enabled: bool| {
        let mut src = bridge_effect_track(1.0);
        src.id = "src".to_string();
        src.inserts[0].id = "src-wdw".to_string();
        src.inserts[0].signal_bus = crate::runtime::SIGNAL_BUS_WDW;
        src.sends = vec![crate::runtime::RuntimeSend {
            id: "send-wdw".to_string(),
            return_track_id: "wdw".to_string(),
            return_track_index: None,
            level: 1.0,
            enabled: send_enabled,
            pre_fader: false,
            level_lane: None,
            enabled_lane: None,
            pdc: Default::default(),
            wdw: true,
        }];
        let mut ret = bridge_effect_track(1.0);
        ret.id = "wdw".to_string();
        ret.track_type = "return".to_string();
        ret.inserts.clear();
        let mut reference = bridge_effect_track(1.0);
        reference.id = "ref".to_string();
        reference.inserts.clear();
        let mut master = bridge_effect_track(1.0);
        master.id = "master".to_string();
        master.track_type = "master".to_string();
        master.inserts.clear();
        let tracks = vec![src, ret, reference, master];
        let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
        let mut p = RuntimeProject {
            spatial: Default::default(),
            tracks,
            audio_graph,
            pdc_enabled: true,
            ..Default::default()
        };
        let sink = WetEffectSink {
            done_seq: AtomicU64::new(1),
            requests: AtomicU64::new(0),
            latency_samples: 960,
        };
        p.plugin_bridge_sinks
            .insert("src-wdw".to_string(), Arc::new(sink));
        p.resolve_bridge_sinks();
        p.plan_latency_and_pdc();
        assert!(p.refresh_runtime_latency_graph(frames));
        assert!(
            !p.refresh_runtime_latency_graph(frames),
            "moving the latency to the return must not keep re-planning"
        );
        p
    };
    // 960 reported + the bridge's one-block handshake.
    let latency = 960 + frames;
    let (src, ret, reference) = (0, 1, 2);

    let p = build(true);
    let lg = &p.latency_graph;
    assert_eq!(p.tracks[src].wdw_return_index, Some(ret));
    assert_eq!(lg.track_plugin_latency[src], latency, "the strip owns it");
    assert_eq!(
        lg.track_block_latency[src], 0,
        "the dry path does not run it"
    );
    assert_eq!(lg.track_block_latency[ret], latency, "the return runs it");
    assert_eq!(lg.max_path_latency_samples, latency);
    // Everything meets at master at the longest path: the dry source and
    // the reference held back by the plug-in's latency, the wet return
    // late by it, its feed undelayed.
    let at_master = |idx: usize| lg.track_output_latency[idx] + lg.track_pdc_delay[idx];
    assert_eq!(lg.track_output_latency[src], 0);
    assert_eq!(at_master(src), latency);
    assert_eq!(at_master(ret), latency);
    assert_eq!(at_master(reference), latency);
    assert_eq!(lg.track_pdc_delay[src], latency);
    assert_eq!(lg.send_pdc_delay[src], vec![0]);

    // Split off: the plug-in is back in the source chain, and so is its
    // latency.
    let p = build(false);
    let lg = &p.latency_graph;
    assert_eq!(p.tracks[src].wdw_return_index, None);
    assert_eq!(lg.track_block_latency[src], latency);
    assert_eq!(lg.track_block_latency[ret], 0);
    assert_eq!(lg.track_pdc_delay[src], 0);
    assert_eq!(lg.track_pdc_delay[reference], latency);
}

/// A multi-out instrument's "Out Ch" strip is fed from the bridged insert,
/// so it carries the plug-in's latency plus one block. Planned as a
/// zero-latency bus it took the full compensation on top and played late
/// by exactly that — an amount that moved with the buffer size.
#[test]
fn multi_out_child_strip_is_compensated_from_its_instrument() {
    let mut inst = bridge_effect_track(1.0);
    inst.id = "inst".to_string();
    inst.inserts[0].id = "inst-vsti".to_string();
    inst.inserts[0].vsti_output_children = vec![crate::runtime::RuntimeVstiOutputChild {
        dest_track_id: "child".to_string(),
        dest_track_index: None,
        bus_index: 1,
        channel_count: 2,
        channel_l: 3,
        channel_r: 4,
    }];
    let mut child = bridge_effect_track(1.0);
    child.id = "child".to_string();
    child.track_type = "bus".to_string();
    child.inserts.clear();
    let mut dry = bridge_effect_track(1.0);
    dry.id = "dry".to_string();
    dry.inserts.clear();
    let mut master = bridge_effect_track(1.0);
    master.id = "master".to_string();
    master.track_type = "master".to_string();
    master.inserts.clear();

    let tracks = vec![inst, child, dry, master];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let mut p = RuntimeProject {
        spatial: Default::default(),
        tracks,
        audio_graph,
        pdc_enabled: true,
        ..Default::default()
    };
    p.resolve_indices();
    let sink = Arc::new(DriftingLatencySink::default());
    sink.latency_samples.store(100, Ordering::Relaxed);
    p.plugin_bridge_sinks
        .insert("inst-vsti".to_string(), sink.clone());
    p.resolve_bridge_sinks();
    p.plan_latency_and_pdc();

    for block in [256u32, 1024] {
        assert!(p.refresh_runtime_latency_graph(block));
        let lg = &p.latency_graph;
        let instrument = 100 + block;
        assert_eq!(lg.track_output_latency[1], instrument, "block {block}");
        assert_eq!(lg.max_path_latency_samples, instrument, "block {block}");
        // The child arrives with the instrument: nothing more to add.
        assert_eq!(lg.track_pdc_delay[1], 0, "block {block}");
        // A dry track waits for it.
        assert_eq!(lg.track_pdc_delay[2], instrument, "block {block}");
    }
}

/// #377: a bridged insert's handshake is charged one *configured* period,
/// so callbacks whose size varies (WASAPI shared/event mode, partial
/// renders) neither replan compensation nor move the dry track's delay.
/// Charged at the callback size, this probe replanned 7 times and the dry
/// delay followed the block size (544 → 96 → 512 → 544 …).
#[test]
fn variable_callback_sizes_keep_bridged_compensation_fixed() {
    const PERIOD: u32 = 480;
    const REPORTED: u32 = 64;
    let mut dry = bridge_effect_track(1.0);
    dry.id = "dry".to_string();
    dry.inserts.clear();
    let mut bridged = bridge_effect_track(1.0);
    bridged.id = "bridged".to_string();
    bridged.inserts[0].id = "bridged-insert".to_string();
    let tracks = vec![dry, bridged];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let mut p = RuntimeProject {
        spatial: Default::default(),
        sample_rate: 48_000,
        tracks,
        audio_graph,
        pdc_enabled: true,
        ..Default::default()
    };
    p.plugin_bridge_sinks.insert(
        "bridged-insert".to_string(),
        Arc::new(WetEffectSink {
            done_seq: AtomicU64::new(1),
            requests: AtomicU64::new(0),
            latency_samples: REPORTED,
        }),
    );
    p.resolve_indices();
    p.plan_latency_and_pdc();
    p.reserve_track_blocks(PERIOD as usize);

    // The device callback path: the stream was opened at PERIOD frames.
    let shared = Arc::new(SharedState::default());
    shared.sample_rate.store(48_000, Ordering::Relaxed);
    shared
        .engine_state
        .store(AudioEngineState::Running as u8, Ordering::Relaxed);
    shared.bridge_period_frames.store(PERIOD, Ordering::Relaxed);
    let mut local = crate::backend::render::LocalAudioState::new(48_000.0);
    local.playing_local = true;
    let mut data = vec![0.0f32; PERIOD as usize * 2];
    let mut callback = |p: &mut RuntimeProject, frames: usize| {
        crate::backend::render::fill_output_f32(&mut data[..frames * 2], 2, p, &shared, &mut local);
    };

    let probe = [480usize, 480, 32, 448, 480, 480, 96, 384, 480];
    for frames in probe {
        callback(&mut p, frames);
        assert_eq!(
            p.latency_graph.track_plugin_latency[1],
            REPORTED + PERIOD,
            "bridged strip after a {frames}-frame block"
        );
        assert_eq!(
            p.latency_graph.track_pdc_delay[0],
            REPORTED + PERIOD,
            "dry delay after a {frames}-frame block"
        );
        assert_eq!(p.latency_graph.track_pdc_delay[1], 0);
    }
    assert_eq!(
        p.latency_replan_count, 1,
        "the plan is computed once, when the bridged latency is first observed"
    );

    // A real change still replans, once: the device reopened at 256.
    shared.bridge_period_frames.store(256, Ordering::Relaxed);
    for frames in probe.map(|frames| frames.min(256)) {
        callback(&mut p, frames);
        assert_eq!(p.latency_graph.track_pdc_delay[0], REPORTED + 256);
    }
    assert_eq!(p.latency_replan_count, 2);

    // A stream that reported no buffer size takes its first block as the
    // period, and keeps it.
    let unconfigured = SharedState::default();
    assert_eq!(unconfigured.bridge_period_frames_for_block(480), 480);
    assert_eq!(unconfigured.bridge_period_frames_for_block(32), 480);
    assert_eq!(unconfigured.bridge_period_frames_for_block(0), 480);
}

/// #408 (follow-up to #377): the stream is opened with the *requested*
/// buffer size (256), but the device negotiates another one and calls
/// back with 480 frames every time. The bridge handshake is one rendered
/// block, so compensation and the latency report must follow the 480 the
/// device delivers, not stay at 256 for the whole session — and follow
/// the device again after it is reopened.
#[test]
fn bridged_compensation_follows_the_negotiated_period() {
    const REQUESTED: u32 = 256;
    const NEGOTIATED: u32 = 480;
    const REPORTED: u32 = 64;
    let mut dry = bridge_effect_track(1.0);
    dry.id = "dry".to_string();
    dry.inserts.clear();
    let mut bridged = bridge_effect_track(1.0);
    bridged.id = "bridged".to_string();
    bridged.inserts[0].id = "bridged-insert".to_string();
    let tracks = vec![dry, bridged];
    let audio_graph = crate::audio_graph::plan_runtime_audio_graph(&tracks).unwrap();
    let mut p = RuntimeProject {
        spatial: Default::default(),
        sample_rate: 48_000,
        tracks,
        audio_graph,
        pdc_enabled: true,
        ..Default::default()
    };
    p.plugin_bridge_sinks.insert(
        "bridged-insert".to_string(),
        Arc::new(WetEffectSink {
            done_seq: AtomicU64::new(1),
            requests: AtomicU64::new(0),
            latency_samples: REPORTED,
        }),
    );
    p.resolve_indices();
    p.plan_latency_and_pdc();
    p.reserve_track_blocks(1024);

    // What `commit_stream_open` publishes for a cpal stream: the size it
    // asked for, since the backend cannot read the negotiated one back.
    let shared = Arc::new(SharedState::default());
    shared.sample_rate.store(48_000, Ordering::Relaxed);
    shared
        .engine_state
        .store(AudioEngineState::Running as u8, Ordering::Relaxed);
    shared
        .bridge_period_frames
        .store(REQUESTED, Ordering::Relaxed);
    let mut local = crate::backend::render::LocalAudioState::new(48_000.0);
    local.playing_local = true;
    let mut data = vec![0.0f32; 1024 * 2];
    let mut callback = |p: &mut RuntimeProject, frames: usize| {
        crate::backend::render::fill_output_f32(&mut data[..frames * 2], 2, p, &shared, &mut local);
    };

    // One second of the device's real callbacks.
    for _ in 0..100 {
        callback(&mut p, NEGOTIATED as usize);
    }
    assert_eq!(
        p.latency_graph.track_plugin_latency[1],
        REPORTED + NEGOTIATED,
        "bridged strip charged the requested period, not the delivered one"
    );
    assert_eq!(p.latency_graph.track_pdc_delay[0], REPORTED + NEGOTIATED);
    assert_eq!(
        shared.bridge_period_frames.load(Ordering::Relaxed),
        NEGOTIATED,
        "the latency report reads the same period"
    );
    let replans = p.latency_replan_count;
    assert!(replans <= 2, "replanned {replans} times");
    // Steady from here on.
    for _ in 0..100 {
        callback(&mut p, NEGOTIATED as usize);
    }
    assert_eq!(p.latency_replan_count, replans);

    // The device is reopened: 128 requested, 441 delivered.
    shared.bridge_period_frames.store(128, Ordering::Relaxed);
    for _ in 0..100 {
        callback(&mut p, 441);
    }
    assert_eq!(p.latency_graph.track_pdc_delay[0], REPORTED + 441);
    assert_eq!(shared.bridge_period_frames.load(Ordering::Relaxed), 441);
}

#[test]
fn external_bridge_effect_stays_dry_without_sink() {
    let mut track = bridge_effect_track(1.0);
    apply_track_chain_block(&mut track, 4, RuntimeTransportContext::default());
    assert!((track.block_l[0] - 1.0).abs() < 1e-6);
    assert!((track.block_r[0] - 1.0).abs() < 1e-6);
}

pub(super) fn bridge_effect_track_with_id(id: &str) -> RuntimeInsert {
    let mut params = HashMap::new();
    params.insert("role".to_string(), serde_json::json!("effect"));
    RuntimeInsert {
        id: id.to_string(),
        kind: "external-bridge-plugin".to_string(),
        kind_tag: crate::runtime::RuntimeInsertKind::ExternalBridge,
        enabled: true,
        bypass_lane: None,
        automation_bypassed: false,
        params: Arc::new(params),
        bridge_is_effect: true,
        bridge_is_builtin: false,
        bridge_enabled_output_channels: Vec::new(),
        bridge_sink: None,
        dsp: InsertDspState::default(),
        vst3: None,
        cpu_us: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        guard: Default::default(),
        callback_process_log_done: false,
        silent_process_blocks: 0,
        bridge_missed_blocks: 0,
        scratch_l: vec![0.0; 8],
        scratch_r: vec![0.0; 8],
        vsti_output_children: Vec::new(),
        scratch_multi: Vec::new(),
        chain_latency_samples: 0,
        signal_bus: 0,
    }
}

#[test]
fn serial_bridge_effect_chain_applies_both_gains() {
    // Models the real one-block handshake: read previous wet → write next
    // dry → request (which processes written dry into the next wet).
    #[derive(Debug)]
    struct MultSink {
        mult: f32,
        fresh: AtomicU64,
        in_l: std::sync::Mutex<Vec<f32>>,
        in_r: std::sync::Mutex<Vec<f32>>,
        out_l: std::sync::Mutex<Vec<f32>>,
        out_r: std::sync::Mutex<Vec<f32>>,
    }

    impl PluginBridgeSink for MultSink {
        fn dsp_ready(&self) -> bool {
            true
        }
        fn read_output(&self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) -> usize {
            if self.fresh.swap(0, Ordering::AcqRel) == 0 {
                return 0;
            }
            let bl = self.out_l.lock().unwrap();
            let br = self.out_r.lock().unwrap();
            let n = frames.min(out_l.len()).min(out_r.len()).min(bl.len());
            out_l[..n].copy_from_slice(&bl[..n]);
            out_r[..n].copy_from_slice(&br[..n]);
            n
        }
        fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}
        fn write_input(&self, in_l: &[f32], in_r: &[f32], frames: usize) {
            let n = frames.min(in_l.len()).min(in_r.len());
            *self.in_l.lock().unwrap() = in_l[..n].to_vec();
            *self.in_r.lock().unwrap() = in_r[..n].to_vec();
        }
        fn request_block(&self, _: u32) {
            let inl = self.in_l.lock().unwrap();
            let inr = self.in_r.lock().unwrap();
            let mut outl = self.out_l.lock().unwrap();
            let mut outr = self.out_r.lock().unwrap();
            outl.resize(inl.len(), 0.0);
            outr.resize(inr.len(), 0.0);
            for i in 0..inl.len() {
                outl[i] = inl[i] * self.mult;
                outr[i] = inr[i] * self.mult;
            }
            self.fresh.store(1, Ordering::Release);
        }
    }

    let mut track = RuntimeTrack {
        spatial: None,
        spatial_params: Default::default(),
        active_voices: 0,
        listen: crate::monitor::ListenMode::Off,
        id: "track-1".to_string(),
        track_type: "audio".to_string(),
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
        inserts: vec![
            bridge_effect_track_with_id("insert-a"),
            bridge_effect_track_with_id("insert-b"),
        ],
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        plugin_param_automation: Vec::new(),
        meter: Arc::new(Default::default()),
        meter_peak_l: 0.0,
        meter_peak_r: 0.0,
        meter_sum_sq_l: 0.0,
        meter_sum_sq_r: 0.0,
        callback_insert_log_done: false,
        callback_clip_route_log_done: false,
        block_l: vec![1.0; 8],
        block_r: vec![1.0; 8],
        recv_l: vec![0.0; 8],
        recv_r: vec![0.0; 8],
        soundfont_l: vec![0.0; 8],
        soundfont_r: vec![0.0; 8],
        ara_renderers: Vec::new(),
        ara_l: vec![0.0; 8],
        ara_r: vec![0.0; 8],
        ara_silence: vec![0.0; 8],
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
    };
    // Pre-seed previous-cycle wet so the first read applies A×2 then B×3.
    let sink_a = Arc::new(MultSink {
        mult: 2.0,
        fresh: AtomicU64::new(1),
        in_l: std::sync::Mutex::new(vec![0.0; 8]),
        in_r: std::sync::Mutex::new(vec![0.0; 8]),
        out_l: std::sync::Mutex::new(vec![2.0; 8]),
        out_r: std::sync::Mutex::new(vec![2.0; 8]),
    });
    let sink_b = Arc::new(MultSink {
        mult: 3.0,
        fresh: AtomicU64::new(1),
        in_l: std::sync::Mutex::new(vec![0.0; 8]),
        in_r: std::sync::Mutex::new(vec![0.0; 8]),
        out_l: std::sync::Mutex::new(vec![6.0; 8]),
        out_r: std::sync::Mutex::new(vec![6.0; 8]),
    });
    track.inserts[0].bridge_sink = Some(sink_a as Arc<dyn PluginBridgeSink>);
    track.inserts[1].bridge_sink = Some(sink_b as Arc<dyn PluginBridgeSink>);
    apply_track_chain_block(&mut track, 4, RuntimeTransportContext::default());
    assert!(
        (track.block_l[0] - 6.0).abs() < 1e-4,
        "serial chain expected 1*2*3=6 got {}",
        track.block_l[0]
    );
}

#[test]
fn serial_bridge_effect_chain_bypasses_only_missed_middle_insert() {
    #[derive(Debug)]
    struct MultSink {
        mult: f32,
        fresh: AtomicU64,
        in_l: std::sync::Mutex<Vec<f32>>,
        in_r: std::sync::Mutex<Vec<f32>>,
        out_l: std::sync::Mutex<Vec<f32>>,
        out_r: std::sync::Mutex<Vec<f32>>,
    }

    impl PluginBridgeSink for MultSink {
        fn dsp_ready(&self) -> bool {
            true
        }
        fn read_output(&self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) -> usize {
            if self.fresh.swap(0, Ordering::AcqRel) == 0 {
                return 0;
            }
            let bl = self.out_l.lock().unwrap();
            let br = self.out_r.lock().unwrap();
            let n = frames.min(out_l.len()).min(out_r.len()).min(bl.len());
            out_l[..n].copy_from_slice(&bl[..n]);
            out_r[..n].copy_from_slice(&br[..n]);
            n
        }
        fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}
        fn write_input(&self, in_l: &[f32], in_r: &[f32], frames: usize) {
            let n = frames.min(in_l.len()).min(in_r.len());
            *self.in_l.lock().unwrap() = in_l[..n].to_vec();
            *self.in_r.lock().unwrap() = in_r[..n].to_vec();
        }
        fn request_block(&self, _: u32) {
            let inl = self.in_l.lock().unwrap();
            let inr = self.in_r.lock().unwrap();
            let mut outl = self.out_l.lock().unwrap();
            let mut outr = self.out_r.lock().unwrap();
            outl.resize(inl.len(), 0.0);
            outr.resize(inr.len(), 0.0);
            for i in 0..inl.len() {
                outl[i] = inl[i] * self.mult;
                outr[i] = inr[i] * self.mult;
            }
            self.fresh.store(1, Ordering::Release);
        }
    }

    #[derive(Debug)]
    struct MissSink;

    impl PluginBridgeSink for MissSink {
        fn dsp_ready(&self) -> bool {
            true
        }
        fn read_output(&self, _: &mut [f32], _: &mut [f32], _: usize) -> usize {
            0
        }
        fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}
        fn write_input(&self, _: &[f32], _: &[f32], _: usize) {}
        fn request_block(&self, _: u32) {}
    }

    let mut track = RuntimeTrack {
        spatial: None,
        spatial_params: Default::default(),
        active_voices: 0,
        listen: crate::monitor::ListenMode::Off,
        id: "track-1".to_string(),
        track_type: "audio".to_string(),
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
        inserts: vec![
            bridge_effect_track_with_id("insert-a"),
            bridge_effect_track_with_id("insert-b"),
            bridge_effect_track_with_id("insert-c"),
        ],
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        plugin_param_automation: Vec::new(),
        meter: Arc::new(Default::default()),
        meter_peak_l: 0.0,
        meter_peak_r: 0.0,
        meter_sum_sq_l: 0.0,
        meter_sum_sq_r: 0.0,
        callback_insert_log_done: false,
        callback_clip_route_log_done: false,
        block_l: vec![1.0; 8],
        block_r: vec![1.0; 8],
        recv_l: vec![0.0; 8],
        recv_r: vec![0.0; 8],
        soundfont_l: vec![0.0; 8],
        soundfont_r: vec![0.0; 8],
        ara_renderers: Vec::new(),
        ara_l: vec![0.0; 8],
        ara_r: vec![0.0; 8],
        ara_silence: vec![0.0; 8],
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
    };
    let sink_a = Arc::new(MultSink {
        mult: 2.0,
        fresh: AtomicU64::new(1),
        in_l: std::sync::Mutex::new(vec![0.0; 8]),
        in_r: std::sync::Mutex::new(vec![0.0; 8]),
        out_l: std::sync::Mutex::new(vec![2.0; 8]),
        out_r: std::sync::Mutex::new(vec![2.0; 8]),
    });
    let sink_b = Arc::new(MissSink);
    let sink_c = Arc::new(MultSink {
        mult: 5.0,
        fresh: AtomicU64::new(1),
        in_l: std::sync::Mutex::new(vec![0.0; 8]),
        in_r: std::sync::Mutex::new(vec![0.0; 8]),
        out_l: std::sync::Mutex::new(vec![10.0; 8]),
        out_r: std::sync::Mutex::new(vec![10.0; 8]),
    });
    track.inserts[0].bridge_sink = Some(sink_a as Arc<dyn PluginBridgeSink>);
    track.inserts[1].bridge_sink = Some(sink_b as Arc<dyn PluginBridgeSink>);
    track.inserts[2].bridge_sink = Some(sink_c as Arc<dyn PluginBridgeSink>);
    apply_track_chain_block(&mut track, 4, RuntimeTransportContext::default());
    assert!(
        (track.block_l[0] - 10.0).abs() < 1e-4,
        "A x2, B missed, C x5 expected 1*2*5=10 got {}",
        track.block_l[0]
    );
}
