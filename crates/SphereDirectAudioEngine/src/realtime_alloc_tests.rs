//! The audio callback's command and MIDI paths must neither allocate nor free.
//!
//! Each test runs the realtime code under
//! [`crate::test_alloc::count_heap_events`], asserts it made no heap
//! allocation or free on the calling thread, and then checks it still did its
//! job — a path made allocation-free by doing nothing would pass the first
//! half alone.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_channel::bounded;

use crate::backend::render::{drain_commands, LocalAudioState};
use crate::command::{EngineCommand, TempoMapChange};
use crate::engine::{AudioEngineState, EngineInner, SharedState, SlowBlock, SlowBlockReport};
use crate::graveyard::test_hooks as graveyard_hooks;
use crate::runtime::{
    build_unresolved_midi_schedule, midi_block_event_capacity, RuntimeMidiData, RuntimeMidiEvent,
    RuntimeMidiEventKind, RuntimeMidiTrack, RuntimeProject,
};
use crate::tempo_map::RuntimeTempoMapSnapshot;
use crate::test_alloc::count_heap_events;
use crate::types::{
    EngineMidiClipSnapshot, EngineMidiNoteSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot,
    EngineSolfegeSnapshot, EngineTrackInputSourceSnapshot, EngineTrackSnapshot,
};

const SR: u32 = 48_000;
const TRACK: &str = "instrument-1";

fn track(id: &str, track_type: &str, instrument: bool) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: track_type.to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: false,
        input_source: EngineTrackInputSourceSnapshot {
            device_id: None,
            channels: Vec::new(),
        },
        preview_mode: "stereo".to_string(),
        output_track_id: None,
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
        // The model-free Solfege instrument: an in-process instrument whose
        // events go through the track's per-block list, with nothing on disk.
        solfege_engine: instrument.then(|| EngineSolfegeSnapshot {
            model_path: None,
            instrument: "Violin".to_string(),
            voice: "Solo Bowed String".to_string(),
            preset: "Test".to_string(),
            bow_pressure: 0.62,
            vibrato: 0.0,
            dynamics: 0.78,
            expression: 1.0,
        }),
    }
}

fn note(pitch: u8, start_beat: f64, length_beats: f64) -> EngineMidiNoteSnapshot {
    EngineMidiNoteSnapshot {
        id: 0,
        pitch,
        start_beat,
        length_beats,
        velocity: 100,
        channel: 0,
        expression: sphere_midi_service::NoteExpression::default(),
        articulation: None,
        pitch_points: Vec::new(),
    }
}

fn snapshot(notes: Vec<EngineMidiNoteSnapshot>) -> EngineProjectSnapshot {
    let notes = notes
        .into_iter()
        .enumerate()
        .map(|(index, mut note)| {
            note.id = index as u64 + 1;
            note
        })
        .collect();
    EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "realtime-alloc-test".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SR,
        tracks: vec![
            track(TRACK, "instrument", true),
            track("master", "master", false),
        ],
        clips: Vec::new(),
        midi_clips: vec![Arc::new(EngineMidiClipSnapshot {
            source_track_id: None,
            id: "clip-1".to_string(),
            track_id: TRACK.to_string(),
            start_beat: 0.0,
            length_beats: 64.0,
            notes,
            controllers: Vec::new(),
            mpe: sphere_midi_service::mpe::MpeTrackConfiguration::default(),
        })],
        pdc_enabled: false,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SR,
            buffer_size: 256,
        },
    }
}

/// `count` short notes packed into the first `span_beats`.
fn dense_notes(count: usize, span_beats: f64) -> Vec<EngineMidiNoteSnapshot> {
    let step = span_beats / count as f64;
    (0..count)
        .map(|i| note(48 + (i % 24) as u8, i as f64 * step, step * 0.5))
        .collect()
}

/// A graph-less project holding `snapshot`'s MIDI schedule at 120 BPM.
fn schedule_only(snapshot: &EngineProjectSnapshot) -> RuntimeProject {
    let data = build_unresolved_midi_schedule(snapshot, SR);
    RuntimeProject {
        sample_rate: SR,
        tempo_map: RuntimeTempoMapSnapshot::static_tempo(120.0),
        midi_clips: data.midi_clips,
        midi_tracks: data.midi_tracks,
        ..Default::default()
    }
}

/// Instrument events the scheduler puts in the track's per-block list for
/// `[0, frames)` (pitch and articulation events travel on their own lists).
fn block_events_expected(runtime: &RuntimeProject, frames: u64) -> usize {
    runtime
        .midi_tracks
        .iter()
        .flat_map(|track| track.events.iter())
        .filter(|event| event.sample < frames)
        .filter(|event| {
            matches!(
                event.kind,
                RuntimeMidiEventKind::NoteOn
                    | RuntimeMidiEventKind::NoteOff
                    | RuntimeMidiEventKind::ControlChange
            )
        })
        .count()
}

fn event_samples(tracks: &[RuntimeMidiTrack]) -> Vec<Vec<(u64, u64)>> {
    tracks
        .iter()
        .map(|track| {
            track
                .events
                .iter()
                .map(|event| (event.sample, event.note_id))
                .collect()
        })
        .collect()
}

/// The counter itself sees heap work, or every zero below would be vacuous.
#[test]
fn the_heap_counter_counts() {
    let (_, heap) = count_heap_events(|| drop(std::hint::black_box(vec![1u8; 16])));
    assert_eq!(heap, 2, "one allocation and one free");
}

// ── #212: LoadProject ──────────────────────────────────────────────────────

/// A graph swap while bridged editors are open, into a graph at another
/// tempo: the open-editor set, the metronome's tempo map and the retired graph
/// all move without a single allocation or free on the callback.
#[test]
fn load_project_swaps_editors_and_tempo_without_heap_work() {
    crate::graveyard::prime();
    let tempo_90 = RuntimeTempoMapSnapshot::static_tempo(90.0);
    let scenario = || {
        let shared = Arc::new(SharedState::default());
        let local = LocalAudioState::new(SR as f64);
        let mut runtime = RuntimeProject {
            sample_rate: SR,
            ..Default::default()
        };
        runtime.bridge_editor_active.insert("track-a".to_string());
        runtime.bridge_editor_active.insert("track-b".to_string());
        let mut next = RuntimeProject {
            sample_rate: SR,
            tempo_map: tempo_90.clone(),
            ..Default::default()
        };
        // What `load_project` packs with every graph it sends.
        next.metronome_tempo_map = Some(next.tempo_map.clone());
        let (tx, rx) = bounded(4);
        tx.send(EngineCommand::LoadProject(Box::new(next))).unwrap();
        (shared, local, runtime, rx)
    };
    // A first pass initialises the process-wide lazy debug flags the arm
    // reads (each reads an env var once), so the counted pass is steady state.
    let (shared, mut local, mut runtime, rx) = scenario();
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);
    let (shared, mut local, mut runtime, rx) = scenario();

    let (_, heap) =
        count_heap_events(|| drain_commands(&rx, &mut runtime, &shared, &mut local, SR));

    assert_eq!(heap, 0, "LoadProject allocated or freed on the callback");
    assert_eq!(runtime.bridge_editor_active.len(), 2);
    assert!(runtime.bridge_editor_active.contains("track-a"));
    assert!(runtime.bridge_editor_active.contains("track-b"));
    assert_eq!(runtime.tempo_map.segments, tempo_90.segments);
    assert_eq!(
        local.tempo_map.segments, tempo_90.segments,
        "metronome follows"
    );
}

// ── #211: SetTempoMap ──────────────────────────────────────────────────────

/// Runs `change` through the callback's command drain on a schedule-only graph
/// of `project` at `position`: once to initialise the process-wide lazy debug
/// flags the path reads (each reads an env var once), then again, counted, on
/// a fresh copy. Returns the counted graph, the transport position and
/// metronome map it left, and the heap events it made.
fn drain_tempo_change(
    project: &EngineProjectSnapshot,
    position: u64,
    change: impl Fn(&RuntimeProject) -> Box<TempoMapChange>,
) -> (RuntimeProject, u64, RuntimeTempoMapSnapshot, u64) {
    crate::graveyard::prime();
    let run = |count: bool| {
        let mut runtime = schedule_only(project);
        let shared = Arc::new(SharedState::default());
        shared.position_samples.store(position, Ordering::Relaxed);
        let mut local = LocalAudioState::new(SR as f64);
        let (tx, rx) = bounded(4);
        tx.send(EngineCommand::SetTempoMap(change(&runtime)))
            .unwrap();
        let heap = if count {
            count_heap_events(|| drain_commands(&rx, &mut runtime, &shared, &mut local, SR)).1
        } else {
            drain_commands(&rx, &mut runtime, &shared, &mut local, SR);
            0
        };
        let position = shared.position_samples.load(Ordering::Relaxed);
        (runtime, position, local.tempo_map.clone(), heap)
    };
    run(false);
    run(true)
}

/// A tempo edit during playback on a project with a few thousand MIDI
/// events: the callback swaps in the schedule the control thread re-timed,
/// with no allocation, free or sort, and lands exactly where re-timing on the
/// callback used to.
#[test]
fn set_tempo_map_installs_a_prepared_schedule_without_heap_work() {
    let project = snapshot(dense_notes(1_500, 48.0));
    assert!(schedule_only(&project).midi_tracks[0].events.len() >= 3_000);
    let position = 5 * SR as u64;
    let tempo = crate::runtime::build_tempo_map_from_points(
        96.0,
        &[crate::types::EngineTempoPointSnapshot {
            beat: 16.0,
            bpm: 140.0,
            curve: 0,
            tension: 0.0,
        }],
    );

    // What the callback used to do, as the reference.
    let mut reference = schedule_only(&project);
    let expected_position = reference.apply_tempo_map(tempo.clone(), position);

    let (runtime, position, metronome_map, heap) = drain_tempo_change(&project, position, |live| {
        // Control thread (`EngineInner::set_tempo_map`): re-time the
        // mirror's copy of the running schedule.
        let mut mirror = RuntimeMidiData {
            midi_clips: live.midi_clips.clone(),
            midi_tracks: live.midi_tracks.clone(),
        };
        mirror.retime(&tempo, SR);
        let mut prepared = mirror.realtime_copy();
        prepared.prepare_block_event_buffers();
        Box::new(TempoMapChange {
            metronome_map: tempo.clone(),
            map: tempo.clone(),
            midi: Some(prepared),
            sample_rate: SR,
        })
    });

    assert_eq!(heap, 0, "SetTempoMap allocated or freed on the callback");
    assert_eq!(position, expected_position);
    assert_eq!(runtime.tempo_map.segments, tempo.segments);
    assert_eq!(metronome_map.segments, tempo.segments, "metronome follows");
    assert_eq!(
        event_samples(&runtime.midi_tracks),
        event_samples(&reference.midi_tracks)
    );
    assert_eq!(
        runtime.midi_tracks[0].cursor,
        reference.midi_tracks[0].cursor
    );
}

/// No prepared schedule (a new stream re-seeded with a map its graph does not
/// carry, or one that drifted from the mirror): the callback re-times its own
/// schedule in place — still without allocating — and gets the same result.
#[test]
fn set_tempo_map_without_a_prepared_schedule_retimes_in_place_without_heap_work() {
    let project = snapshot(dense_notes(1_500, 48.0));
    let position = 3 * SR as u64;
    let tempo = RuntimeTempoMapSnapshot::static_tempo(73.0);
    let mut reference = schedule_only(&project);
    let expected_position = reference.apply_tempo_map(tempo.clone(), position);

    let (runtime, position, _, heap) = drain_tempo_change(&project, position, |_| {
        Box::new(TempoMapChange {
            metronome_map: tempo.clone(),
            map: tempo.clone(),
            midi: None,
            sample_rate: 0,
        })
    });

    assert_eq!(heap, 0, "in-place re-time allocated or freed");
    assert_eq!(position, expected_position);
    assert_eq!(
        event_samples(&runtime.midi_tracks),
        event_samples(&reference.midi_tracks)
    );
}
// ── #238: the first tempo command after a device open ──────────────────────

/// Heap events, graveyard drop-threads spawned and retired values leaked on
/// the calling thread while the callback drains `cmd`.
struct CallbackCost {
    heap: u64,
    spawns: u64,
    leaks: u64,
}

/// Play the callback for one command: no warm-up pass, so anything the
/// callback still initialises lazily — a graveyard channel and its thread, a
/// cached debug flag — shows up here, as it would on the first real block.
fn callback_drains(
    cmd: EngineCommand,
    runtime: &mut RuntimeProject,
    shared: &Arc<SharedState>,
    local: &mut LocalAudioState,
) -> CallbackCost {
    let (tx, rx) = bounded(1);
    tx.send(cmd).unwrap();
    let spawns = graveyard_hooks::worker_spawns_on_this_thread();
    let leaks = graveyard_hooks::leaks_on_this_thread();
    let (_, heap) = count_heap_events(|| drain_commands(&rx, runtime, shared, local, SR));
    CallbackCost {
        heap,
        spawns: graveyard_hooks::worker_spawns_on_this_thread() - spawns,
        leaks: graveyard_hooks::leaks_on_this_thread() - leaks,
    }
}

fn assert_realtime_clean(cost: CallbackCost, what: &str) {
    assert_eq!(
        cost.spawns, 0,
        "{what}: spawned a graveyard thread on the callback"
    );
    assert_eq!(cost.heap, 0, "{what}: allocated or freed on the callback");
    assert_eq!(cost.leaks, 0, "{what}: had no graveyard to retire into");
}

/// The Jam order: the engine is created and a device opened before any
/// project is loaded. The re-seed's `SetTempoMap` is the callback's first
/// tempo command and its first use of the tempo graveyard; it must find the
/// graveyard already running. A user tempo edit that follows is clean too.
#[test]
fn first_tempo_command_after_engine_creation_and_device_open_is_realtime_clean() {
    let engine = EngineInner::new();
    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);

    let first = rx
        .try_recv()
        .expect("the device open re-seeds the callback");
    assert!(matches!(first, EngineCommand::SetTempoMap(_)));
    let cost = callback_drains(first, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "the re-seed's SetTempoMap");
    assert!(crate::graveyard::is_primed());
    // The rest of the re-seed (time signature, metronome) is not under test.
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    engine.set_tempo_map(90.0, Vec::new()).unwrap();
    let edit = rx.try_recv().expect("a tempo edit reaches the callback");
    let cost = callback_drains(edit, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "a tempo edit");
    let tempo_90 = RuntimeTempoMapSnapshot::static_tempo(90.0);
    assert_eq!(runtime.tempo_map.segments, tempo_90.segments);
    assert_eq!(
        local.tempo_map.segments, tempo_90.segments,
        "metronome follows"
    );
}

/// Close and reopen the device with nothing in between that would have
/// initialised the graveyard on the control thread: the reopened callback's
/// first tempo command is still clean.
#[test]
fn first_tempo_command_after_a_device_reopen_is_realtime_clean() {
    let engine = EngineInner::new();
    let (first_stream, _) = engine.open_test_stream(SR);
    drop(first_stream);
    engine.close_device();
    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);

    let first = rx.try_recv().expect("the reopen re-seeds the callback");
    assert!(matches!(first, EngineCommand::SetTempoMap(_)));
    let cost = callback_drains(first, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "the reopen's SetTempoMap");
}

/// The in-place re-time keeps the schedule order the stable sort gives, even
/// where the new tempo pulls apart events that shared a sample and were
/// ordered by kind (a note-off just after a note-on, same sample at 120 BPM).
#[test]
fn in_place_retime_matches_a_stable_sort() {
    let event = |beat: f64, kind: RuntimeMidiEventKind, note_id: u64| RuntimeMidiEvent {
        sample: 0,
        beat,
        kind,
        pitch: 60,
        velocity: 100,
        channel: 0,
        note_id,
        cc_number: 0,
        cc_value: 0.0,
        pitch_hz: 0.0,
        source: None,
    };
    let at_120 = RuntimeTempoMapSnapshot::static_tempo(120.0);
    let at_30 = RuntimeTempoMapSnapshot::static_tempo(30.0);
    let mut events = vec![
        event(1.0, RuntimeMidiEventKind::NoteOn, 1),
        event(1.000_01, RuntimeMidiEventKind::NoteOff, 2),
        event(1.000_01, RuntimeMidiEventKind::NoteOn, 3),
        event(2.0, RuntimeMidiEventKind::ControlChange, 4),
    ];
    let mut data = RuntimeMidiData {
        midi_clips: Vec::new(),
        midi_tracks: vec![RuntimeMidiTrack {
            events: Vec::new(),
            ..Default::default()
        }],
    };
    // Sorted at 120 BPM: the two first events share sample 24 000, so the
    // note-off of the second leads.
    for e in &mut events {
        e.sample = at_120.samples_at_beat(e.beat, SR as f64);
    }
    data.midi_tracks[0].events = events.clone();
    data.retime(&at_120, SR);
    let order = |data: &RuntimeMidiData| -> Vec<u64> {
        data.midi_tracks[0]
            .events
            .iter()
            .map(|e| e.note_id)
            .collect()
    };
    assert_eq!(order(&data), vec![2, 1, 3, 4]);

    data.retime(&at_30, SR);
    let mut expected = events;
    for e in &mut expected {
        e.sample = at_30.samples_at_beat(e.beat, SR as f64);
    }
    expected.sort_by(|a, b| {
        a.sample.cmp(&b.sample).then(
            matches!(b.kind, RuntimeMidiEventKind::NoteOff)
                .cmp(&matches!(a.kind, RuntimeMidiEventKind::NoteOff)),
        )
    });
    assert_eq!(order(&data), vec![1, 2, 3, 4]);
    assert_eq!(
        order(&data),
        expected.iter().map(|e| e.note_id).collect::<Vec<_>>()
    );
}

// ── #152: per-block instrument MIDI ────────────────────────────────────────

/// Schedule one block and undo it: initialises the lazy debug flags the
/// scheduler reads, so the counted pass that follows is steady state.
fn warm_up_scheduler(runtime: &mut RuntimeProject, block: u64) {
    runtime.schedule_midi_block(0, block);
    runtime.reset_midi_playback(0);
    for track in &mut runtime.tracks {
        track.midi_block_events.clear();
        track.solfege_pitch_events.clear();
        track.solfege_articulation_events.clear();
    }
}

/// A 4096-frame block with ~800 instrument events on an in-process
/// instrument: the list the control thread reserved holds all of them, and
/// scheduling the block neither grows it nor drops any.
#[test]
fn a_dense_block_fits_the_list_reserved_off_the_audio_thread() {
    const BLOCK: u64 = 4096;
    // 400 notes in 0.08 beats (1 920 frames at 120 BPM): 800 events.
    let project = snapshot(dense_notes(400, 0.08));
    let mut runtime = RuntimeProject::build(&project, SR, &mut HashMap::new(), None, true)
        .expect("runtime builds");
    runtime.reserve_track_blocks(BLOCK as usize);
    let expected = block_events_expected(&runtime, BLOCK);
    assert!(
        expected > 256,
        "{expected} events must overflow the old fixed list"
    );
    warm_up_scheduler(&mut runtime, BLOCK);

    let (_, heap) = count_heap_events(|| runtime.schedule_midi_block(0, BLOCK));

    assert_eq!(heap, 0, "scheduling grew the block list on the callback");
    let track = runtime.tracks.iter().find(|t| t.id == TRACK).unwrap();
    assert_eq!(track.midi_block_events.len(), expected, "no event dropped");
}

/// A note edit that makes the schedule denser than the running graph's list:
/// the schedule brings a larger list with it, the swap adopts it, and the
/// dense block then schedules without heap work.
#[test]
fn a_denser_schedule_brings_its_own_block_list() {
    const BLOCK: u64 = 4096;
    crate::graveyard::prime();
    let sparse = snapshot(vec![note(60, 0.0, 1.0)]);
    let mut runtime = RuntimeProject::build(&sparse, SR, &mut HashMap::new(), None, true)
        .expect("runtime builds");
    runtime.reserve_track_blocks(BLOCK as usize);
    warm_up_scheduler(&mut runtime, BLOCK);
    let base_capacity = runtime
        .tracks
        .iter()
        .find(|t| t.id == TRACK)
        .unwrap()
        .midi_block_events
        .capacity();

    let dense = snapshot(dense_notes(400, 0.08));
    let mut schedule = runtime.build_midi_schedule(&dense);
    schedule.prepare_block_event_buffers();

    let (_, heap) = count_heap_events(|| runtime.replace_midi_schedule(&mut schedule, 0));
    assert_eq!(heap, 0, "ReplaceMidi allocated or freed on the callback");
    drop(schedule);
    let adopted = runtime
        .tracks
        .iter()
        .find(|t| t.id == TRACK)
        .unwrap()
        .midi_block_events
        .capacity();
    assert!(adopted > base_capacity, "{adopted} <= {base_capacity}");

    let expected = block_events_expected(&runtime, BLOCK);
    assert!(expected > 256);
    warm_up_scheduler(&mut runtime, BLOCK);
    let (_, heap) = count_heap_events(|| runtime.schedule_midi_block(0, BLOCK));
    assert_eq!(heap, 0, "scheduling grew the block list on the callback");
    let track = runtime.tracks.iter().find(|t| t.id == TRACK).unwrap();
    assert_eq!(track.midi_block_events.len(), expected, "no event dropped");
}

#[test]
fn block_list_capacity_covers_two_dense_windows_and_the_panic() {
    let events: Vec<RuntimeMidiEvent> = (0..600u64)
        .map(|i| RuntimeMidiEvent {
            sample: i * 10,
            beat: 0.0,
            kind: RuntimeMidiEventKind::ControlChange,
            pitch: 0,
            velocity: 0,
            channel: 0,
            note_id: 0,
            cc_number: 1,
            cc_value: 0.0,
            pitch_hz: 0.0,
            source: None,
        })
        .collect();
    // 100 events in any 1 000-frame window.
    let capacity = midi_block_event_capacity(&events, 1_000);
    assert_eq!(capacity, 2 * 100 + 128 + 64 + 32);
    // A sparse schedule keeps the base list.
    assert_eq!(midi_block_event_capacity(&events[..2], 1_000), 256);
}

// ── #162: slow-block watchdog ──────────────────────────────────────────────

/// The callback's side of the watchdog is three stores and an increment; the
/// line is formatted and written by whoever takes the report.
#[test]
fn slow_block_reports_are_published_without_heap_work_and_taken_once() {
    let report = SlowBlockReport::default();
    assert!(report.take().is_none());
    let block = SlowBlock {
        elapsed_us: 4_100,
        deadline_us: 5_333,
        frames: 256,
        state: AudioEngineState::Running,
    };
    let (_, heap) = count_heap_events(|| report.publish(block));
    assert_eq!(heap, 0);
    let (taken, count) = report.take().expect("published");
    assert_eq!(taken, block);
    assert_eq!(count, 1);
    assert_eq!(taken.severity(), "warning");
    assert_eq!(taken.load_pct(), 76);
    assert!(report.take().is_none(), "taken once");

    // Several between two drains: the latest one, and how many there were.
    for elapsed_us in [5_000, 6_000, 7_000] {
        report.publish(SlowBlock {
            elapsed_us,
            ..block
        });
    }
    let (taken, count) = report.take().unwrap();
    assert_eq!(taken.elapsed_us, 7_000);
    assert_eq!(taken.severity(), "error");
    assert_eq!(count, 3);
}

// ── #312: time signature map changes ──────────────────────────────────────

#[test]
fn first_time_signature_command_after_engine_creation_and_device_open_is_realtime_clean() {
    let engine = EngineInner::new();
    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);

    let first = rx
        .try_recv()
        .expect("the device open re-seeds the callback with tempo map");
    assert!(matches!(first, EngineCommand::SetTempoMap(_)));
    let _ = callback_drains(first, &mut runtime, &shared, &mut local);

    let second = rx
        .try_recv()
        .expect("the device open re-seeds the callback with time signature map");
    assert!(matches!(second, EngineCommand::SetTimeSignatureMap(_)));
    let cost = callback_drains(second, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "the re-seed's SetTimeSignatureMap");
    assert!(crate::graveyard::is_primed());
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    engine.set_time_signature(3, 4).unwrap();
    let edit = rx
        .try_recv()
        .expect("a time signature edit reaches the callback");
    let cost = callback_drains(edit, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "a time signature edit");
    assert_eq!(shared.time_sig_num.load(Ordering::Relaxed), 3);
    assert_eq!(shared.time_sig_den.load(Ordering::Relaxed), 4);
    assert_eq!(local.time_signature_map.points()[0].numerator, 3);
    assert_eq!(local.time_signature_map.points()[0].denominator, 4);
}

#[test]
fn first_time_signature_command_after_a_device_reopen_is_realtime_clean() {
    let engine = EngineInner::new();
    let (first_stream, _) = engine.open_test_stream(SR);
    drop(first_stream);
    engine.close_device();
    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);

    let first = rx.try_recv().expect("the reopen re-seeds the callback");
    assert!(matches!(first, EngineCommand::SetTempoMap(_)));
    let _ = callback_drains(first, &mut runtime, &shared, &mut local);

    let second = rx.try_recv().expect("the reopen's SetTimeSignatureMap");
    assert!(matches!(second, EngineCommand::SetTimeSignatureMap(_)));
    let cost = callback_drains(second, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "the reopen's SetTimeSignatureMap");
}

#[test]
fn time_signature_edit_during_playback_is_realtime_clean() {
    let engine = EngineInner::new();
    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    engine
        .set_time_signature_map(vec![
            crate::time_signature_map::RuntimeTimeSignaturePointSnapshot {
                beat: 0.0,
                numerator: 6,
                denominator: 8,
                grouping: Vec::new(),
            },
            crate::time_signature_map::RuntimeTimeSignaturePointSnapshot {
                beat: 16.0,
                numerator: 7,
                denominator: 8,
                grouping: Vec::new(),
            },
        ])
        .unwrap();

    let edit = rx
        .try_recv()
        .expect("time signature map reaches the callback");
    let cost = callback_drains(edit, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "time signature map edit during playback");
    assert_eq!(local.time_signature_map.points().len(), 2);
    assert_eq!(local.time_signature_map.points()[0].numerator, 6);
    assert_eq!(local.time_signature_map.points()[1].numerator, 7);
}

// ── #291: plug-in parameter moves and bridge editors ──────────────────────

#[test]
fn moving_builtin_plugin_parameter_is_realtime_clean() {
    let engine = EngineInner::new();
    let mut proj = snapshot(Vec::new());
    let mut params = HashMap::new();
    params.insert("drive".to_string(), serde_json::Value::from(0.2));
    proj.tracks[0]
        .inserts
        .push(crate::types::EngineInsertSnapshot {
            id: "drive-1".to_string(),
            kind: "sphere.drive".to_string(),
            enabled: true,
            params,
            state: None,
        });
    engine.load_project(proj).unwrap();

    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    engine
        .set_insert_param(
            TRACK.to_string(),
            "drive-1".to_string(),
            "drive".to_string(),
            0.8,
        )
        .unwrap();
    let cmd = rx.try_recv().expect("insert param command");
    let cost = callback_drains(cmd, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "moving a builtin plugin parameter");

    let insert = &runtime.tracks[0].inserts[0];
    assert_eq!(
        insert.params.get("drive"),
        Some(&serde_json::Value::from(0.8f32 as f64))
    );
}

#[test]
fn moving_builtin_eq_parameter_rebuilds_dsp_is_realtime_clean() {
    let engine = EngineInner::new();
    let mut proj = snapshot(Vec::new());
    let mut params = HashMap::new();
    params.insert("band1Gain".to_string(), serde_json::Value::from(0.0));
    proj.tracks[0]
        .inserts
        .push(crate::types::EngineInsertSnapshot {
            id: "eq-1".to_string(),
            kind: "sphere.eq8".to_string(),
            enabled: true,
            params,
            state: None,
        });
    engine.load_project(proj).unwrap();

    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    engine
        .set_insert_param(
            TRACK.to_string(),
            "eq-1".to_string(),
            "band1Gain".to_string(),
            3.5,
        )
        .unwrap();
    let cmd = rx.try_recv().expect("insert param command");
    let cost = callback_drains(cmd, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "moving an eq parameter that rebuilds DSP state");

    let insert = &runtime.tracks[0].inserts[0];
    assert_eq!(
        insert.params.get("band1Gain"),
        Some(&serde_json::Value::from(3.5f32 as f64))
    );
}

#[test]
fn toggling_insert_bypass_is_realtime_clean() {
    let engine = EngineInner::new();
    let mut proj = snapshot(Vec::new());
    let mut params = HashMap::new();
    params.insert("drive".to_string(), serde_json::Value::from(0.2));
    proj.tracks[0]
        .inserts
        .push(crate::types::EngineInsertSnapshot {
            id: "drive-1".to_string(),
            kind: "sphere.drive".to_string(),
            enabled: true,
            params,
            state: None,
        });
    engine.load_project(proj).unwrap();

    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    // Bypass insert (value < 0.5)
    engine
        .set_insert_param(
            TRACK.to_string(),
            "drive-1".to_string(),
            "enabled".to_string(),
            0.0,
        )
        .unwrap();
    let cmd = rx.try_recv().expect("bypass command");
    let cost = callback_drains(cmd, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "bypassing an insert");
    assert!(!runtime.tracks[0].inserts[0].enabled);

    // Re-enable insert (value >= 0.5)
    engine
        .set_insert_param(
            TRACK.to_string(),
            "drive-1".to_string(),
            "enabled".to_string(),
            1.0,
        )
        .unwrap();
    let cmd = rx.try_recv().expect("re-enable command");
    let cost = callback_drains(cmd, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "re-enabling an insert");
    assert!(runtime.tracks[0].inserts[0].enabled);
}

#[test]
fn moving_bridged_plugin_parameter_is_realtime_clean() {
    let engine = EngineInner::new();
    let mut proj = snapshot(Vec::new());
    let params = HashMap::new();
    proj.tracks[0]
        .inserts
        .push(crate::types::EngineInsertSnapshot {
            id: "bridge-1".to_string(),
            kind: "external-bridge-plugin".to_string(),
            enabled: true,
            params,
            state: None,
        });
    engine.load_project(proj).unwrap();

    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    engine
        .set_insert_param(
            TRACK.to_string(),
            "bridge-1".to_string(),
            "42".to_string(),
            0.75,
        )
        .unwrap();
    let cmd = rx.try_recv().expect("bridged insert param command");
    let cost = callback_drains(cmd, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "moving a bridged plugin parameter");
}

#[test]
fn opening_and_closing_bridged_editor_is_realtime_clean() {
    let engine = EngineInner::new();
    let (rx, mut runtime) = engine.open_test_stream(SR);
    let shared = Arc::clone(&engine.shared);
    let mut local = LocalAudioState::new(SR as f64);
    drain_commands(&rx, &mut runtime, &shared, &mut local, SR);

    // Opening
    engine
        .set_bridge_editor_active(TRACK.to_string(), true)
        .unwrap();
    let open_cmd = rx.try_recv().expect("open editor command");
    let cost = callback_drains(open_cmd, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "opening a bridged editor");
    assert!(runtime.bridge_editor_active.contains(TRACK));

    // Closing
    engine
        .set_bridge_editor_active(TRACK.to_string(), false)
        .unwrap();
    let close_cmd = rx.try_recv().expect("close editor command");
    let cost = callback_drains(close_cmd, &mut runtime, &shared, &mut local);
    assert_realtime_clean(cost, "closing a bridged editor");
    assert!(!runtime.bridge_editor_active.contains(TRACK));
}
