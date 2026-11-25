//! A backend switch must not move any track against the timeline.
//!
//! Each test plays the callback by hand around `EngineInner::open_test_stream`
//! — the control-thread steps every backend open takes, without a device — and
//! locates impulses in the rendered output to the sample.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_channel::Receiver;

use super::EngineInner;
use crate::audio_file::AudioFileBuffer;
use crate::audio_source::ClipAudioSource;
use crate::backend::render::{drain_commands, fill_output_f32, LocalAudioState};
use crate::command::EngineCommand;
use crate::plugin_bridge::PluginBridgeSink;
use crate::runtime::RuntimeProject;
use crate::types::{
    EngineClipSnapshot, EngineInsertSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot,
    EngineTrackInputSourceSnapshot, EngineTrackSnapshot,
};

const BPM: f64 = 120.0;
/// Where the clips of [`aligned`] start: beat 8 at 120 BPM is second 4.
const CLIP_BEAT: f64 = 8.0;
/// Rate the impulse files were recorded at.
const SOURCE_RATE: u32 = 48_000;
/// One impulse level per track; every subset has a distinct sum.
const LEVELS: [f32; 3] = [0.1, 0.2, 0.4];
const ALL_LEVELS: f32 = 0.7;

fn track(id: &str, track_type: &str) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        vca_gain: 1.0,
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        dual_pan: Default::default(),
        muted: false,
        solo: false,
        armed: false,
        input_monitor: false,
        input_source: EngineTrackInputSourceSnapshot::default(),
        preview_mode: "stereo".to_string(),
        output_track_id: None,
        midi_fx: Vec::new(),
        inserts: Vec::new(),
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        builtin_soundfont_player: false,
        soundfont_path: None,
        soundfont_preset_bank: None,
        soundfont_preset_patch: None,
        soundfont_volume: 1.0,
        soundfont_reverb_chorus: true,
        soundfont_polyphony: 64,
        soundfont_envelope: Default::default(),
        soundfont_quality: Default::default(),
        soundfont_mode: Default::default(),
        soundfont_channels: Default::default(),
        solfege_engine: None,
    }
}

fn impulse_path(index: usize) -> String {
    format!("impulse-{index}.wav")
}

/// Three audio tracks, each with one clip whose first source frame is an
/// impulse at that track's level, starting at `beats[track]`.
fn project(output_rate: u32, beats: [f64; 3]) -> EngineProjectSnapshot {
    let mut tracks: Vec<_> = (0..LEVELS.len())
        .map(|index| track(&format!("audio-{index}"), "audio"))
        .collect();
    tracks.push(track("master", "master"));
    let clips = beats
        .iter()
        .enumerate()
        .map(|(index, beat)| EngineClipSnapshot {
            id: format!("clip-{index}"),
            track_id: format!("audio-{index}"),
            asset_id: format!("asset-{index}"),
            media_path: Some(impulse_path(index)),
            start_beat: *beat,
            duration_beats: 2.0,
            offset_seconds: 0.0,
            gain: 1.0,
            muted: false,
            ara_rendered: false,
            fades: None,
            stretch: Default::default(),
            audio_process: None,
        })
        .collect();
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "backend-switch-sync".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: BPM,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: output_rate,
        tracks,
        clips,
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: output_rate,
            buffer_size: 256,
        },
    }
}

/// Every track's clip at [`CLIP_BEAT`].
fn aligned(output_rate: u32) -> EngineProjectSnapshot {
    project(output_rate, [CLIP_BEAT; 3])
}

fn gain_insert(id: &str) -> EngineInsertSnapshot {
    let mut params = HashMap::new();
    params.insert("gainDb".to_string(), serde_json::json!(0.0));
    EngineInsertSnapshot {
        id: id.to_string(),
        kind: "gain".to_string(),
        enabled: true,
        params,
        state: None,
    }
}

fn bridged_insert(id: &str) -> EngineInsertSnapshot {
    let mut params = HashMap::new();
    params.insert("role".to_string(), serde_json::json!("effect"));
    EngineInsertSnapshot {
        id: id.to_string(),
        kind: "external-bridge-plugin".to_string(),
        enabled: true,
        params,
        state: None,
    }
}

/// The timeline sample `beat` falls on at `rate`.
fn beat_sample(beat: f64, rate: u32) -> u64 {
    (beat * 60.0 / BPM * rate as f64).round() as u64
}

/// An engine whose media cache already holds the impulse files, so no test
/// touches the disk.
fn engine_with_impulses() -> EngineInner {
    let engine = EngineInner::new();
    let frames = SOURCE_RATE as usize;
    let mut cache = engine.audio_cache.lock();
    for (index, level) in LEVELS.iter().enumerate() {
        let mut samples = vec![0.0f32; frames * 2];
        samples[0] = *level;
        samples[1] = *level;
        cache.insert(
            impulse_path(index),
            Arc::new(ClipAudioSource::InMemory(Arc::new(AudioFileBuffer {
                sample_rate: SOURCE_RATE,
                channels: 2,
                frames,
                samples,
            }))),
        );
    }
    drop(cache);
    engine
}

/// The callback half of an open stream: what a backend owns between
/// `prepare_stream_open` and the device asking for blocks.
struct TestStream {
    rx: Receiver<EngineCommand>,
    runtime: RuntimeProject,
    local: LocalAudioState,
    rate: u32,
}

impl TestStream {
    /// Open a stream at `rate` that reports `period` frames per block
    /// (0: it reports none), replacing whichever stream was open.
    fn open(engine: &EngineInner, rate: u32, period: u32) -> Self {
        let (rx, runtime) = engine.open_test_stream(rate);
        engine.shared.set_stream_bridge_period(period);
        Self {
            rx,
            runtime,
            local: LocalAudioState::new(rate as f64),
            rate,
        }
    }

    /// The Settings reopen (`open_daux_safe`): the transport flag is cleared
    /// and the previous stream is gone before the next one is built.
    fn reopen(self, engine: &EngineInner, rate: u32, period: u32) -> Self {
        engine.shared.playing.store(false, Ordering::Relaxed);
        drop(self);
        Self::open(engine, rate, period)
    }

    /// One device callback of `frames`: drain commands, render. Returns the
    /// transport sample the block started at and its left channel.
    fn block(&mut self, engine: &EngineInner, frames: usize) -> (u64, Vec<f32>) {
        drain_commands(
            &self.rx,
            &mut self.runtime,
            &engine.shared,
            &mut self.local,
            self.rate,
        );
        let start = engine.shared.position_samples.load(Ordering::Relaxed);
        let mut data = vec![0.0f32; frames * 2];
        fill_output_f32(
            &mut data,
            2,
            &mut self.runtime,
            &engine.shared,
            &mut self.local,
        );
        (start, data.chunks(2).map(|frame| frame[0]).collect())
    }

    /// Render `seconds` in blocks cycling through `sizes` and return every
    /// non-silent sample as `(timeline sample, value)`.
    fn render(&mut self, engine: &EngineInner, sizes: &[usize], seconds: f64) -> Vec<(u64, f32)> {
        let mut heard = Vec::new();
        let mut remaining = (seconds * self.rate as f64).ceil() as i64;
        for frames in sizes.iter().cycle() {
            if remaining <= 0 {
                break;
            }
            remaining -= *frames as i64;
            let (start, left) = self.block(engine, *frames);
            for (offset, value) in left.iter().enumerate() {
                if value.abs() > 1.0e-4 {
                    heard.push((start + offset as u64, *value));
                }
            }
        }
        heard
    }

    /// Seek to `from` seconds, play `seconds`, stop.
    fn play_from(
        &mut self,
        engine: &EngineInner,
        sizes: &[usize],
        from: f64,
        seconds: f64,
    ) -> Vec<(u64, f32)> {
        engine.seek(from).unwrap();
        engine.play().unwrap();
        let heard = self.render(engine, sizes, seconds);
        self.stop(engine, sizes[0]);
        heard
    }

    fn stop(&mut self, engine: &EngineInner, frames: usize) {
        engine.pause().unwrap();
        self.block(engine, frames);
    }
}

#[track_caller]
fn assert_impulses(what: &str, heard: &[(u64, f32)], expected: &[(u64, f32)]) {
    let matches = heard.len() == expected.len()
        && heard
            .iter()
            .zip(expected)
            .all(|(got, want)| got.0 == want.0 && (got.1 - want.1).abs() < 1.0e-3);
    assert!(matches, "{what}: heard {heard:?}, expected {expected:?}");
}

/// A bridged effect as the host runs it: each exchange hands back the block
/// the previous exchange asked for, passed through `latency` samples of delay.
#[derive(Debug)]
struct DelayBridgeSink {
    latency: u32,
    state: parking_lot::Mutex<DelayBridgeState>,
}

#[derive(Debug, Default)]
struct DelayBridgeState {
    line: VecDeque<f32>,
    input: Vec<f32>,
    ready: Option<Vec<f32>>,
}

impl DelayBridgeSink {
    fn new(latency: u32) -> Arc<Self> {
        Arc::new(Self {
            latency,
            state: parking_lot::Mutex::new(DelayBridgeState {
                line: std::iter::repeat(0.0).take(latency as usize).collect(),
                ..Default::default()
            }),
        })
    }
}

impl PluginBridgeSink for DelayBridgeSink {
    fn dsp_ready(&self) -> bool {
        true
    }
    fn read_output(&self, out_l: &mut [f32], out_r: &mut [f32], frames: usize) -> usize {
        let Some(block) = self.state.lock().ready.take() else {
            return 0;
        };
        let n = frames.min(block.len());
        out_l[..n].copy_from_slice(&block[..n]);
        out_r[..n].copy_from_slice(&block[..n]);
        n
    }
    fn push_midi(&self, _: u8, _: u8, _: u8, _: u32) {}
    fn write_input(&self, in_l: &[f32], _: &[f32], frames: usize) {
        self.state.lock().input = in_l[..frames].to_vec();
    }
    fn request_block(&self, frames: u32) {
        let mut state = self.state.lock();
        let mut input = std::mem::take(&mut state.input);
        input.resize(frames as usize, 0.0);
        let block = input
            .into_iter()
            .map(|sample| {
                state.line.push_back(sample);
                state.line.pop_front().unwrap_or(0.0)
            })
            .collect();
        state.ready = Some(block);
    }
    fn reported_latency_samples(&self) -> u32 {
        self.latency
    }
}

/// `(rate, frames per callback)` of each stream a test switches through:
/// other buffer sizes at one rate, and other rates.
const SWITCHES: [(u32, usize); 6] = [
    (48_000, 480),
    (48_000, 64),
    (44_100, 441),
    (48_000, 1024),
    (32_000, 256),
    (48_000, 128),
];

/// Play `snapshot` on every stream of [`SWITCHES`] in turn — as the stream
/// comes up from the engine's own graph, and again after the project sync the
/// Studio sends behind a reopen — and require the three impulses on one
/// timeline sample each time.
fn assert_aligned_through_switches(snapshot: impl Fn(u32) -> EngineProjectSnapshot) {
    let engine = engine_with_impulses();
    let (rate, frames) = SWITCHES[0];
    let mut stream = TestStream::open(&engine, rate, frames as u32);
    engine.load_project(snapshot(rate)).unwrap();
    for (rate, frames) in SWITCHES {
        stream = stream.reopen(&engine, rate, frames as u32);
        let at = beat_sample(CLIP_BEAT, rate);
        for resynced in [false, true] {
            if resynced {
                engine.load_project(snapshot(rate)).unwrap();
            }
            let heard = stream.play_from(&engine, &[frames], 3.95, 0.1);
            assert_impulses(
                &format!("{rate} Hz / {frames} frames, resynced={resynced}"),
                &heard,
                &[(at, ALL_LEVELS)],
            );
        }
    }
}

/// Test A, D, E: three plain audio tracks through repeated switches of buffer
/// size and rate.
#[test]
fn plain_tracks_stay_aligned_through_backend_switches() {
    assert_aligned_through_switches(aligned);
}

/// Test B: the same with a different built-in chain on each track.
#[test]
fn tracks_with_builtin_chains_stay_aligned_through_backend_switches() {
    assert_aligned_through_switches(|rate| {
        let mut snapshot = aligned(rate);
        snapshot.tracks[1].inserts = vec![gain_insert("gain-1a")];
        snapshot.tracks[2].inserts = vec![gain_insert("gain-2a"), gain_insert("gain-2b")];
        snapshot
    });
}

/// Test C: two tracks behind bridged plug-ins reporting different latencies
/// and a dry one. After every switch all three land on one sample, the
/// longest path (plug-in latency plus one bridge period) after the clip start
/// — including on a stream that reports one period and delivers another.
#[test]
fn bridged_latencies_stay_compensated_through_backend_switches() {
    const LATENCIES: [u32; 2] = [64, 300];
    let snapshot = |rate: u32| {
        let mut snapshot = aligned(rate);
        snapshot.tracks[0].inserts = vec![bridged_insert("bridged-0")];
        snapshot.tracks[1].inserts = vec![bridged_insert("bridged-1")];
        snapshot
    };
    let engine = engine_with_impulses();
    let mut stream = TestStream::open(&engine, 48_000, 480);
    engine.load_project(snapshot(48_000)).unwrap();
    for (index, latency) in LATENCIES.iter().enumerate() {
        engine
            .set_plugin_bridge_sink(
                format!("bridged-{index}"),
                Some(DelayBridgeSink::new(*latency)),
            )
            .unwrap();
    }
    // `(rate, period the stream reports, frames it delivers)`.
    let switches = [
        (48_000, 480, 480),
        (48_000, 128, 128),
        (48_000, 256, 480),
        (44_100, 441, 441),
        (48_000, 64, 64),
    ];
    for (rate, reported, frames) in switches {
        stream = stream.reopen(&engine, rate, reported);
        let at = beat_sample(CLIP_BEAT, rate) + u64::from(LATENCIES[1]) + frames as u64;
        for resynced in [false, true] {
            if resynced {
                engine.load_project(snapshot(rate)).unwrap();
            }
            let heard = stream.play_from(&engine, &[frames], 3.5, 0.7);
            assert_impulses(
                &format!("{rate} Hz / {frames} frames, resynced={resynced}"),
                &heard,
                &[(at, ALL_LEVELS)],
            );
        }
    }
}

/// An ARA plug-in's latency is part of its track's path whether or not the
/// track also has inserts, and stays part of it when the graph is brought to
/// another rate. It used to be dropped twice: by the first compensation
/// refresh on a track with inserts (which summed the inserts alone), and by
/// the retarget on every track.
#[test]
fn ara_latency_survives_the_refresh_and_a_rate_change() {
    const ARA_WITH_INSERT: u32 = 100;
    const ARA_ALONE: u32 = 50;
    const REPORTED: u32 = 64;
    let engine = engine_with_impulses();
    let mut stream = TestStream::open(&engine, 48_000, 480);
    let mut snapshot = aligned(48_000);
    snapshot.tracks[0].inserts = vec![bridged_insert("bridged-0")];
    engine.load_project(snapshot).unwrap();
    engine
        .set_plugin_bridge_sink(
            "bridged-0".to_string(),
            Some(DelayBridgeSink::new(REPORTED)),
        )
        .unwrap();
    stream.block(&engine, 480);
    // What `set_ara_renderers` leaves on a track: its renderers' latency, on
    // its own and as part of the track's.
    for (index, ara) in [(0, ARA_WITH_INSERT), (1, ARA_ALONE)] {
        let track = &mut stream.runtime.tracks[index];
        track.ara_latency_samples = ara;
        track.plugin_latency_samples += ara;
    }

    for _ in 0..4 {
        stream.block(&engine, 480);
    }
    let plan = &stream.runtime.latency_graph;
    let longest = ARA_WITH_INSERT + REPORTED + 480;
    assert_eq!(plan.track_plugin_latency[0], longest);
    assert_eq!(plan.track_plugin_latency[1], ARA_ALONE);
    assert_eq!(plan.track_pdc_delay[1], longest - ARA_ALONE);
    assert_eq!(plan.track_pdc_delay[2], longest, "the dry track waits");

    stream.runtime.retarget_sample_rate(44_100);
    let plan = &stream.runtime.latency_graph;
    assert_eq!(plan.track_plugin_latency[1], ARA_ALONE);
    assert!(plan.track_plugin_latency[0] >= ARA_WITH_INSERT);
    stream.runtime.refresh_runtime_latency_graph(441);
    let plan = &stream.runtime.latency_graph;
    assert_eq!(
        plan.track_plugin_latency[0],
        ARA_WITH_INSERT + REPORTED + 441
    );
    assert_eq!(plan.track_plugin_latency[1], ARA_ALONE);
}

/// Test F: the transport stays at the same place in the song across a switch.
/// The position and the loop are counted in device samples, so a stream at
/// another rate has to be handed them converted; a stream at the same rate
/// gets them untouched. Reopened under a transport flag that was left set,
/// the new stream holds the position until it is started.
#[test]
fn transport_position_and_loop_survive_a_rate_change() {
    let engine = engine_with_impulses();
    let position = || engine.shared.position_samples.load(Ordering::Relaxed);
    let loop_bounds = || {
        (
            engine.shared.loop_start_samples.load(Ordering::Relaxed),
            engine.shared.loop_end_samples.load(Ordering::Relaxed),
        )
    };
    let mut stream = TestStream::open(&engine, 48_000, 480);
    engine.load_project(aligned(48_000)).unwrap();
    engine.set_loop(false, 2.0, 6.0).unwrap();
    engine.seek(4.25).unwrap();
    stream.block(&engine, 480);
    assert_eq!(position(), 204_000);
    assert_eq!(loop_bounds(), (96_000, 288_000));

    let stream = stream.reopen(&engine, 44_100, 441);
    assert_eq!(position(), 187_425, "4.25 s at 44.1 kHz");
    assert_eq!(loop_bounds(), (88_200, 264_600));

    // A plain reopen does not clear the flag; the stream still waits.
    engine.shared.playing.store(true, Ordering::Relaxed);
    drop(stream);
    let mut stream = TestStream::open(&engine, 44_100, 128);
    assert_eq!(position(), 187_425, "same rate: untouched");
    for _ in 0..4 {
        stream.block(&engine, 128);
    }
    assert_eq!(position(), 187_425, "not started: not advanced");

    stream.reopen(&engine, 48_000, 480);
    assert_eq!(position(), 204_000);
    assert_eq!(loop_bounds(), (96_000, 288_000));
}

/// A project load that read the stream's rate, built its graph, and finished
/// after the stream had been replaced by one at another rate. The callback
/// relabels the graph it is handed, so every clip started at its 48 kHz
/// sample count read at 44.1 kHz — 8.8% of its position late, each track by a
/// different amount. The load now brings the graph to the open stream's rate.
#[test]
fn a_load_built_for_the_replaced_stream_plays_in_time() {
    let beats = [8.0, 8.5, 9.0];
    let engine = engine_with_impulses();
    let stream = TestStream::open(&engine, 48_000, 480);
    engine.load_project(aligned(48_000)).unwrap();
    let mut stream = stream.reopen(&engine, 44_100, 441);

    engine
        .load_project_built_at(
            project(48_000, beats),
            engine.insert_param_revision(),
            48_000,
        )
        .unwrap();

    assert_eq!(engine.runtime.lock().sample_rate, 44_100);
    let heard = stream.play_from(&engine, &[441], 3.95, 0.7);
    let expected: Vec<_> = beats
        .iter()
        .zip(LEVELS)
        .map(|(beat, level)| (beat_sample(*beat, 44_100), level))
        .collect();
    assert_impulses("staggered clips at 44.1 kHz", &heard, &expected);
}

/// Test G: half a minute of playback in callbacks of changing size, with a
/// backend switch to another rate in the middle and no seek after it. Clips
/// 16 and 26 seconds after the first still start on their own sample.
#[test]
fn no_drift_over_long_playback_across_a_switch() {
    let beats = [8.0, 40.0, 60.0];
    let sizes = [480, 64, 441, 512, 33];
    let engine = engine_with_impulses();
    let mut stream = TestStream::open(&engine, 48_000, 480);
    engine.load_project(project(48_000, beats)).unwrap();

    let heard = stream.play_from(&engine, &sizes, 3.9, 11.0);
    assert_impulses(
        "before the switch",
        &heard,
        &[(beat_sample(beats[0], 48_000), LEVELS[0])],
    );
    let stopped_at = engine.shared.position_samples.load(Ordering::Relaxed);

    let mut stream = stream.reopen(&engine, 44_100, 441);
    engine.load_project(project(44_100, beats)).unwrap();
    let resumed_at = engine.shared.position_samples.load(Ordering::Relaxed);
    assert_eq!(
        resumed_at,
        super::lifecycle::rescale_samples(stopped_at, 48_000, 44_100)
    );

    // Play on from where the transport stopped: no seek.
    engine.play().unwrap();
    let heard = stream.render(&engine, &sizes, 16.0);
    assert_impulses(
        "after the switch",
        &heard,
        &[
            (beat_sample(beats[1], 44_100), LEVELS[1]),
            (beat_sample(beats[2], 44_100), LEVELS[2]),
        ],
    );
}

/// Three tracks: two behind bridged plug-ins of `latencies`, one dry.
fn bridged_project(rate: u32) -> EngineProjectSnapshot {
    let mut snapshot = aligned(rate);
    snapshot.tracks[0].inserts = vec![bridged_insert("bridged-0")];
    snapshot.tracks[1].inserts = vec![bridged_insert("bridged-1")];
    snapshot
}

fn attach_delay_sinks(engine: &EngineInner, latencies: [u32; 2]) {
    for (index, latency) in latencies.iter().enumerate() {
        engine
            .set_plugin_bridge_sink(
                format!("bridged-{index}"),
                Some(DelayBridgeSink::new(*latency)),
            )
            .unwrap();
    }
}

/// A project sync while playing (any edit: an insert added, a clip's gain)
/// swaps in a rebuilt graph. The impulses are already inside the compensation
/// delay lines when it lands, and must still come out of them, on time: the
/// rebuilt graph's lines used to start empty, so every compensated track
/// played its delay's worth of silence after each edit.
#[test]
fn a_sync_during_playback_keeps_what_the_delay_lines_hold() {
    const LATENCIES: [u32; 2] = [64, 300];
    const FRAMES: usize = 480;
    let engine = engine_with_impulses();
    let mut stream = TestStream::open(&engine, 48_000, FRAMES as u32);
    engine.load_project(bridged_project(48_000)).unwrap();
    attach_delay_sinks(&engine, LATENCIES);
    engine.load_project(bridged_project(48_000)).unwrap();

    let clip = beat_sample(CLIP_BEAT, 48_000);
    let at = clip + u64::from(LATENCIES[1]) + FRAMES as u64;
    engine.seek(3.5).unwrap();
    engine.play().unwrap();
    // Play to one block past the clip start: every impulse has gone into
    // the plug-ins or the delay lines, and none has come out yet.
    let mut heard = stream.render(&engine, &[FRAMES], 0.51);
    let position = engine.shared.position_samples.load(Ordering::Relaxed);
    assert!(position > clip && position < at, "position {position}");

    // A structural edit, so the graph is rebuilt and swapped: a track added.
    let mut edited = bridged_project(48_000);
    edited.tracks.insert(3, track("audio-extra", "audio"));
    engine.load_project(edited).unwrap();
    heard.extend(stream.render(&engine, &[FRAMES], 0.2));
    stream.stop(&engine, FRAMES);
    assert_impulses("across a sync", &heard, &[(at, ALL_LEVELS)]);
}

/// A bridged plug-in whose latency is longer than the delay lines the graph
/// was built with: attached after the build, as every plug-in is. The
/// callback cannot lengthen the lines itself, so the dry tracks played
/// undelayed — and stayed that way through every later sync, since an
/// unchanged sync keeps the graph and a rebuild planned without the plug-in's
/// latency again. The status poll now rebuilds it once, planned with it.
#[test]
fn a_long_bridged_latency_is_compensated_once_the_status_poll_runs() {
    const LATENCIES: [u32; 2] = [9_000, 0];
    const FRAMES: usize = 256;
    let engine = engine_with_impulses();
    let mut stream = TestStream::open(&engine, 48_000, FRAMES as u32);
    engine.load_project(bridged_project(48_000)).unwrap();
    attach_delay_sinks(&engine, LATENCIES);
    // The callback finds a latency the graph was not built for…
    stream.block(&engine, FRAMES);
    assert!(engine.shared.pdc_rings_short.load(Ordering::Relaxed));
    // …and the control thread's regular visit rebuilds it.
    engine.get_daux_status();
    assert!(!engine.shared.pdc_rings_short.load(Ordering::Relaxed));

    let at = beat_sample(CLIP_BEAT, 48_000) + u64::from(LATENCIES[0]) + FRAMES as u64;
    for sync in 0..3 {
        engine.load_project(bridged_project(48_000)).unwrap();
        let heard = stream.play_from(&engine, &[FRAMES], 3.9, 0.5);
        assert_impulses(&format!("after sync {sync}"), &heard, &[(at, ALL_LEVELS)]);
    }
    assert!(!engine.shared.pdc_rings_short.load(Ordering::Relaxed));
}

/// A rebuilt graph is planned with the latency its plug-ins report, so the
/// swap leaves the plan as it was and the callback has nothing to replan.
#[test]
fn a_rebuilt_graph_is_planned_with_its_plugins_latency() {
    const LATENCIES: [u32; 2] = [64, 300];
    const FRAMES: usize = 480;
    let engine = engine_with_impulses();
    let mut stream = TestStream::open(&engine, 48_000, FRAMES as u32);
    engine.load_project(bridged_project(48_000)).unwrap();
    attach_delay_sinks(&engine, LATENCIES);
    stream.block(&engine, FRAMES);
    let planned = stream.runtime.latency_graph.track_pdc_delay.clone();

    // A structural edit: one more (dry) track.
    let mut edited = bridged_project(48_000);
    edited.tracks.insert(3, track("audio-extra", "audio"));
    engine.load_project(edited).unwrap();
    stream.block(&engine, FRAMES);
    assert_eq!(
        stream.runtime.latency_replan_count, 0,
        "replanned at the swap"
    );
    assert_eq!(
        &stream.runtime.latency_graph.track_pdc_delay[..3],
        &planned[..3]
    );
}

/// Low-latency monitoring: an armed track's own latent effect and the
/// master's are taken out of its path once their latency is over the limit,
/// and kept when it is not; a track that is not live keeps everything.
#[test]
fn low_latency_monitoring_takes_latent_effects_out_of_a_live_path() {
    const TRACK_LATENCY: u32 = 300;
    const MASTER_LATENCY: u32 = 600;
    const FRAMES: usize = 480;
    let snapshot = || {
        let mut snapshot = aligned(48_000);
        snapshot.tracks[0].inserts = vec![bridged_insert("bridged-0")];
        snapshot.tracks[3].inserts = vec![bridged_insert("bridged-1")];
        snapshot
    };
    let clip = beat_sample(CLIP_BEAT, 48_000);
    let bridged = |latency: u32| u64::from(latency) + FRAMES as u64;
    // (limit, track 0 armed, where the three impulses land together)
    let cases = [
        (
            0,
            true,
            clip + bridged(TRACK_LATENCY) + bridged(MASTER_LATENCY),
        ),
        (256, true, clip),
        (512, true, clip + bridged(TRACK_LATENCY)),
        (
            256,
            false,
            clip + bridged(TRACK_LATENCY) + bridged(MASTER_LATENCY),
        ),
    ];
    for (limit, armed, at) in cases {
        let engine = engine_with_impulses();
        let mut stream = TestStream::open(&engine, 48_000, FRAMES as u32);
        engine.load_project(snapshot()).unwrap();
        attach_delay_sinks(&engine, [TRACK_LATENCY, MASTER_LATENCY]);
        stream.block(&engine, FRAMES);
        // Armed on the graph itself: arming through the project would open
        // the input device.
        stream.runtime.tracks[0].record_armed = armed;
        engine.set_low_latency_monitoring(limit);
        let heard = stream.play_from(&engine, &[FRAMES], 3.9, 0.3);
        assert_impulses(
            &format!("limit {limit}, armed {armed}"),
            &heard,
            &[(at, ALL_LEVELS)],
        );
    }
}

/// Correct Takes for Interface Latency: on, a take moves earlier by the round
/// trip measured during it; off, it stays where it was captured, for the
/// Recording Delay alone to place.
#[test]
fn a_take_moves_by_the_interface_round_trip_only_when_asked_to() {
    let engine = engine_with_impulses();
    let _stream = TestStream::open(&engine, 48_000, 256);
    engine.load_project(aligned(48_000)).unwrap();
    let place = |engine: &EngineInner| {
        let take = crate::types::JsRecordingResult {
            track_id: "audio-0".to_string(),
            file_path: String::new(),
            relative_path: String::new(),
            start_beat: CLIP_BEAT,
            duration_seconds: 1.0,
            sample_rate: 48_000,
            channels: 2,
            metadata_path: String::new(),
            sample_format: "s32le".to_string(),
            // 480 samples at 48 kHz: 0.02 beat at 120 BPM.
            latency_seconds: 0.01,
            success: true,
            error: None,
        };
        let placement = super::TakePlacement {
            armed_start_sample: 0,
            first_capture_sample: u64::MAX,
            live_tap: false,
            loopback_tap: false,
            pdc_bypassed: false,
            #[cfg(all(target_os = "windows", feature = "asio"))]
            asio_tap: false,
        };
        engine.finish_stop_recording(vec![take], placement)[0].start_beat
    };
    let corrected = place(&engine);
    assert!(
        (corrected - (CLIP_BEAT - 0.02)).abs() < 1.0e-9,
        "{corrected}"
    );
    engine.set_record_interface_latency_correction(false);
    let uncorrected = place(&engine);
    assert!((uncorrected - CLIP_BEAT).abs() < 1.0e-9, "{uncorrected}");
}
