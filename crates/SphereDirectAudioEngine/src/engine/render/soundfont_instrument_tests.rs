//! End-to-end coverage for the built-in Soundfont Player as a track
//! instrument: a real `.sf2` is loaded through the runtime graph, MIDI reaches
//! it the same way the piano roll and the Soundfont Player window send it, and
//! the rendered block is checked for actual audio.

use super::render_project_block_interleaved;
use crate::runtime::RuntimeProject;
use crate::types::{
    EngineMidiClipSnapshot, EngineMidiNoteSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot,
    EngineTrackSnapshot,
};
use sphere_soundfont_player::{
    test_font, SoundfontEnvelope, SoundfontRenderQuality, DECIMATOR_LATENCY_SAMPLES,
};
use std::collections::HashMap;
use std::path::PathBuf;

const SAMPLE_RATE: u32 = 48_000;
const FRAMES: usize = 512;

/// A per-test `.sf2` on disk, removed when the guard drops.
struct FontFile {
    dir: PathBuf,
    path: PathBuf,
}

impl FontFile {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "futureboard-soundfont-engine-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("test.sf2");
        test_font::write_sf2(&path).expect("write test soundfont");
        Self { dir, path }
    }
}

impl Drop for FontFile {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn soundfont_track(id: &str, font: &FontFile, preset: (i32, i32)) -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: id.to_string(),
        track_type: "instrument".to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: false,
        input_source: Default::default(),
        preview_mode: "stereo".to_string(),
        output_track_id: None,
        inserts: Vec::new(),
        sends: Vec::new(),
        automation_lanes: Vec::new(),
        builtin_soundfont_player: true,
        soundfont_path: Some(font.path.to_string_lossy().into_owned()),
        soundfont_preset_bank: Some(preset.0),
        soundfont_preset_patch: Some(preset.1),
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

fn master_track() -> EngineTrackSnapshot {
    EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: "master".to_string(),
        track_type: "master".to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        armed: false,
        input_monitor: false,
        input_source: Default::default(),
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
        solfege_engine: None,
    }
}

fn runtime(font: &FontFile, preset: (i32, i32)) -> RuntimeProject {
    runtime_with_shaping(font, preset, Default::default(), Default::default())
}

fn runtime_with_shaping(
    font: &FontFile,
    preset: (i32, i32),
    envelope: SoundfontEnvelope,
    quality: SoundfontRenderQuality,
) -> RuntimeProject {
    let mut track = soundfont_track("sf-1", font, preset);
    track.soundfont_envelope = envelope;
    track.soundfont_quality = quality;
    build_runtime(vec![track, master_track()])
}

fn build_runtime(tracks: Vec<EngineTrackSnapshot>) -> RuntimeProject {
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "soundfont-test".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SAMPLE_RATE,
        tracks,
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SAMPLE_RATE,
            buffer_size: FRAMES as u32,
        },
    };
    RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
        .expect("soundfont runtime builds")
}

/// Renders one stopped-transport block — the state the Soundfont Player
/// window's Test button and the piano roll audition run in — and returns
/// its peak level.
fn render_peak(runtime: &mut RuntimeProject) -> f32 {
    let mut output = vec![0.0f32; FRAMES * 2];
    render_project_block_interleaved(runtime, 0, 1.0, &mut output, 2, false, 4, 4, None);
    output.iter().fold(0.0f32, |peak, s| peak.max(s.abs()))
}

fn multi_runtime(
    font: &FontFile,
    parts: sphere_soundfont_player::SoundfontChannels,
) -> RuntimeProject {
    let mut track = soundfont_track("sf-1", font, test_font::MELODIC_PRESET);
    track.soundfont_mode = sphere_soundfont_player::SoundfontPlayerMode::Multi;
    track.soundfont_channels = parts;
    build_runtime(vec![track, master_track()])
}

fn melodic_parts() -> sphere_soundfont_player::SoundfontChannels {
    let mut parts = sphere_soundfont_player::default_channels();
    for part in parts.iter_mut() {
        part.preset = Some(test_font::MELODIC_PRESET);
    }
    parts
}

#[test]
fn a_live_part_edit_reaches_the_running_player() {
    let font = FontFile::new("multi-live");
    let mut parts = melodic_parts();
    let mut runtime = multi_runtime(&font, parts);
    runtime.midi_preview_note_on("sf-1", 5, 60, 100);
    assert!(
        render_peak(&mut runtime) > 0.001,
        "channel 6 plays its part"
    );
    runtime.midi_preview_all_notes_off("sf-1");
    for _ in 0..200 {
        render_peak(&mut runtime);
    }

    parts[5].mute = true;
    runtime.update_soundfont_channels(0, &parts);
    runtime.midi_preview_note_on("sf-1", 5, 60, 100);
    assert!(
        render_peak(&mut runtime) < 1.0e-4,
        "a muted part stays silent"
    );
    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    assert!(
        render_peak(&mut runtime) > 0.001,
        "the other parts still play"
    );
}

#[test]
fn a_graph_clone_keeps_the_parts() {
    let font = FontFile::new("multi-clone");
    let mut parts = melodic_parts();
    parts[3].preset = Some(test_font::DRUM_PRESET);
    let runtime = multi_runtime(&font, parts);
    let cloned = runtime.tracks[0]
        .soundfont_player
        .clone()
        .expect("soundfont player");
    let player = cloned.player.as_ref().expect("font loaded");
    assert!(
        player.is_drum_channel(3),
        "the kit on channel 4 survives the clone"
    );
    assert_eq!(player.channels()[3].preset, Some(test_font::DRUM_PRESET));
}

#[test]
fn snapshot_load_attaches_a_player_with_the_requested_preset() {
    let font = FontFile::new("load");
    let runtime = runtime(&font, test_font::MELODIC_PRESET);
    let soundfont = runtime.tracks[0]
        .soundfont_player
        .as_ref()
        .expect("track carries a soundfont player");
    let player = soundfont.player.as_ref().expect("font loaded");
    assert_eq!(player.bank_name(), test_font::BANK_NAME);
    assert_eq!(soundfont.preset, Some(test_font::MELODIC_PRESET));
}

#[test]
fn preview_note_renders_without_an_instrument_insert() {
    let font = FontFile::new("preview");
    let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
    assert!(
        runtime.tracks[0].midi_instrument_insert_ix.is_none(),
        "the built-in player is a track instrument, not an insert"
    );
    assert_eq!(render_peak(&mut runtime), 0.0, "silent before any note");

    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    assert!(
        runtime.has_active_midi_preview(),
        "a soundfont preview note must register so the callback keeps rendering"
    );
    assert!(
        render_peak(&mut runtime) > 0.001,
        "preview note should be audible"
    );

    runtime.midi_preview_note_off("sf-1", 0, 60);
    assert!(!runtime.has_active_midi_preview());
}

#[test]
fn a_drum_bank_preset_sounds_for_a_note_on_the_tracks_own_channel() {
    // Regression: choosing a bank-128 kit program-changed only MIDI channel
    // 10, but an instrument track's notes carry channel 1, so the track was
    // silent (or played a leftover melodic preset) no matter what the panel
    // showed. The player now routes the track's notes to the percussion
    // channel that kit lives on.
    let font = FontFile::new("drum-routing");
    let mut runtime = runtime(&font, test_font::DRUM_PRESET);
    let player = runtime.tracks[0]
        .soundfont_player
        .as_ref()
        .and_then(|sf| sf.player.as_ref())
        .expect("font loaded");
    assert_eq!(player.selected_preset(), Some(test_font::DRUM_PRESET));
    assert_eq!(
        player.routed_channel(0),
        sphere_soundfont_player::PERCUSSION_CHANNEL
    );

    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    assert!(
        render_peak(&mut runtime) > 0.001,
        "a drum kit must sound for a note written on channel 1"
    );
}

#[test]
fn a_melodic_preset_sounds_for_a_note_written_on_channel_ten() {
    let font = FontFile::new("melodic-routing");
    let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
    runtime.midi_preview_note_on("sf-1", sphere_soundfont_player::PERCUSSION_CHANNEL, 60, 100);
    assert!(
        render_peak(&mut runtime) > 0.001,
        "a melodic preset must still play a note that arrives on channel 10"
    );
}

#[test]
fn the_track_envelope_reaches_the_engine_player_and_shapes_playback() {
    let font = FontFile::new("envelope");
    let envelope = SoundfontEnvelope {
        attack_ms: 400.0,
        ..SoundfontEnvelope::default()
    };
    let mut shaped = runtime_with_shaping(
        &font,
        test_font::MELODIC_PRESET,
        envelope,
        SoundfontRenderQuality::Standard,
    );
    assert_eq!(
        shaped.tracks[0]
            .soundfont_player
            .as_ref()
            .and_then(|sf| sf.player.as_ref())
            .expect("font loaded")
            .envelope(),
        envelope,
        "the snapshot's envelope must reach the audible player, not just the window"
    );

    let mut plain = runtime(&font, test_font::MELODIC_PRESET);
    shaped.midi_preview_note_on("sf-1", 0, 60, 100);
    plain.midi_preview_note_on("sf-1", 0, 60, 100);
    // One 512-frame block at 48 kHz is ~11 ms, far inside a 400 ms attack.
    let shaped_peak = render_peak(&mut shaped);
    let plain_peak = render_peak(&mut plain);
    assert!(
        shaped_peak < plain_peak * 0.25,
        "attack should fade playback in: shaped={shaped_peak} plain={plain_peak}"
    );
}

#[test]
fn an_oversampled_quality_still_plays_and_reports_its_latency() {
    let font = FontFile::new("quality");
    let mut runtime = runtime_with_shaping(
        &font,
        test_font::MELODIC_PRESET,
        SoundfontEnvelope::default(),
        SoundfontRenderQuality::High,
    );
    let soundfont = runtime.tracks[0]
        .soundfont_player
        .as_ref()
        .expect("track carries a soundfont player");
    assert_eq!(soundfont.quality, SoundfontRenderQuality::High);
    let player = soundfont.player.as_ref().expect("font loaded");
    assert_eq!(player.sample_rate(), SAMPLE_RATE as i32, "output rate");
    assert_eq!(player.internal_sample_rate(), SAMPLE_RATE as i32 * 2);
    assert_eq!(soundfont.latency_samples(), DECIMATOR_LATENCY_SAMPLES);
    assert_eq!(
        runtime.tracks[0].plugin_latency_samples, DECIMATOR_LATENCY_SAMPLES,
        "the decimation delay has to reach delay compensation"
    );

    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    // The filter's 16-sample delay lands well inside the first block.
    assert!(
        render_peak(&mut runtime) > 0.001,
        "an oversampled player must still be audible"
    );
}

#[test]
fn standard_quality_adds_no_latency() {
    let font = FontFile::new("quality-standard");
    let runtime = runtime(&font, test_font::MELODIC_PRESET);
    assert_eq!(runtime.tracks[0].plugin_latency_samples, 0);
}

#[test]
fn preview_note_plays_on_a_channel_other_than_the_first() {
    // Tracks can put each note on its own MIDI channel, so the selected
    // preset has to be live on every melodic channel, not only channel 1.
    let font = FontFile::new("channels");
    let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
    runtime.midi_preview_note_on("sf-1", 7, 64, 110);
    assert!(render_peak(&mut runtime) > 0.001);
}

#[test]
fn muted_track_renders_no_soundfont_audio() {
    let font = FontFile::new("muted");
    let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
    runtime.tracks[0].muted = true;
    runtime.fader_smoothing = false;
    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    assert!(render_peak(&mut runtime) < 1.0e-6);
}

/// One block plus the per-callback `midi_block_events` clear the real
/// backends do after rendering, so a queued preview note is delivered once
/// rather than re-triggered every block.
fn render_peak_consuming_events(runtime: &mut RuntimeProject) -> f32 {
    let peak = render_peak(runtime);
    for track in &mut runtime.tracks {
        track.midi_block_events.clear();
    }
    peak
}

#[test]
fn a_muted_instrument_track_keeps_its_voices_running_underneath() {
    // Mute/solo silence the output, not the instrument. Under the old skip
    // the muted track never processed its block, so the note-on was
    // dropped with that block's events and unmuting produced silence until
    // the next note. Now the voice runs (and its attack advances) under the
    // mute, so lifting it lands mid-note.
    let font = FontFile::new("mute-continuity");
    let envelope = SoundfontEnvelope {
        attack_ms: 250.0,
        ..SoundfontEnvelope::default()
    };
    let mut muted = runtime_with_shaping(
        &font,
        test_font::MELODIC_PRESET,
        envelope,
        SoundfontRenderQuality::Standard,
    );
    muted.fader_smoothing = false;
    muted.tracks[0].muted = true;
    muted.midi_preview_note_on("sf-1", 0, 60, 100);

    // ~0.25 s of muted playback: nothing audible, but the note is running.
    let blocks = (SAMPLE_RATE as f32 * 0.25 / FRAMES as f32).ceil() as usize;
    for _ in 0..blocks {
        assert!(
            render_peak_consuming_events(&mut muted) < 1.0e-6,
            "a muted track must stay silent"
        );
    }
    muted.tracks[0].muted = false;
    let resumed = render_peak_consuming_events(&mut muted);
    assert!(resumed > 0.001, "unmuting must resume the sounding note");

    // Same note started fresh at that moment would still be inside its
    // attack, so "resumed mid-note" has to be clearly louder than "restarted".
    let mut restarted = runtime_with_shaping(
        &font,
        test_font::MELODIC_PRESET,
        envelope,
        SoundfontRenderQuality::Standard,
    );
    restarted.fader_smoothing = false;
    restarted.midi_preview_note_on("sf-1", 0, 60, 100);
    let first_block = render_peak_consuming_events(&mut restarted);
    assert!(
        resumed > first_block * 2.0,
        "the muted voice must have advanced its attack: resumed={resumed} restart={first_block}"
    );
}

#[test]
fn a_soloed_track_does_not_stop_another_tracks_notes() {
    let font = FontFile::new("solo-continuity");
    let mut runtime = build_runtime(vec![
        soundfont_track("sf-1", &font, test_font::MELODIC_PRESET),
        soundfont_track("sf-2", &font, test_font::MELODIC_PRESET),
        master_track(),
    ]);
    runtime.fader_smoothing = false;

    // Solo the *other* track, then start a note on this one — exactly what
    // happens when the transport rolls into a new phrase while something
    // else is soloed. The old skip threw the note-on away with the block's
    // events, so releasing solo produced silence until the next note.
    runtime.update_track_solo("sf-2", true);
    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    assert!(
        render_peak_consuming_events(&mut runtime) < 1.0e-6,
        "an unsoloed track must be silent"
    );

    runtime.update_track_solo("sf-2", false);
    assert!(
        render_peak_consuming_events(&mut runtime) > 0.001,
        "releasing solo must reveal the note that started under it"
    );
}

#[test]
fn graph_clone_reuses_the_parsed_font_and_still_plays() {
    let font = FontFile::new("clone");
    let runtime = runtime(&font, test_font::MELODIC_PRESET);
    let original = runtime.tracks[0]
        .soundfont_player
        .as_ref()
        .and_then(|sf| sf.player.as_ref())
        .expect("font loaded")
        .sound_font();

    let mut cloned = runtime.clone();
    let clone_font = cloned.tracks[0]
        .soundfont_player
        .as_ref()
        .and_then(|sf| sf.player.as_ref())
        .expect("clone keeps a loaded player")
        .sound_font();
    assert!(
        std::sync::Arc::ptr_eq(&original, &clone_font),
        "a graph swap must not re-parse the SoundFont"
    );

    cloned.midi_preview_note_on("sf-1", 0, 60, 100);
    assert!(render_peak(&mut cloned) > 0.001);
}

/// Renders a whole bar of transport playback offline and reports the peak
/// level in each 0.25-beat slice, so a test can assert *when* the notes of
/// a MIDI clip sound rather than only that something did.
fn render_bar_envelope(runtime: &mut RuntimeProject, beats: f64, bpm: f64) -> Vec<f32> {
    let samples_per_beat = SAMPLE_RATE as f64 * 60.0 / bpm;
    let total = (samples_per_beat * beats) as u64;
    let mut envelope = Vec::new();
    let mut position = 0u64;
    let slice = (samples_per_beat * 0.25) as u64;
    while position < total {
        let mut slice_peak = 0.0f32;
        let slice_end = (position + slice).min(total);
        while position < slice_end {
            let frames = FRAMES.min((slice_end - position) as usize);
            let mut output = vec![0.0f32; frames * 2];
            crate::engine::schedule_midi_render_block(runtime, position, frames as u64, None);
            render_project_block_interleaved(
                runtime,
                position,
                1.0,
                &mut output,
                2,
                true,
                4,
                4,
                None,
            );
            slice_peak = output.iter().fold(slice_peak, |peak, s| peak.max(s.abs()));
            position += frames as u64;
        }
        envelope.push(slice_peak);
    }
    envelope
}

#[test]
fn midi_clip_notes_play_through_the_soundfont_during_transport() {
    // The arrangement path: a MIDI clip on a Soundfont Player track, played
    // by the transport rather than auditioned. Two notes a bar apart, so
    // the rendered envelope has to be silent before the first, loud on each
    // note, and quiet in the gap between them.
    let font = FontFile::new("clip");
    let bpm = 120.0;
    let mut snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "soundfont-clip".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SAMPLE_RATE,
        tracks: vec![
            soundfont_track("sf-1", &font, test_font::MELODIC_PRESET),
            master_track(),
        ],
        clips: Vec::new(),
        midi_clips: vec![std::sync::Arc::new(EngineMidiClipSnapshot {
            source_track_id: None,
            id: "clip-1".to_string(),
            track_id: "sf-1".to_string(),
            start_beat: 1.0,
            length_beats: 4.0,
            notes: vec![
                EngineMidiNoteSnapshot {
                    id: 1,
                    pitch: 60,
                    start_beat: 0.0,
                    length_beats: 0.5,
                    velocity: 100,
                    channel: 0,
                    expression: sphere_midi_service::NoteExpression::default(),
                    pitch_points: Vec::new(),
                    articulation: None,
                },
                EngineMidiNoteSnapshot {
                    id: 2,
                    pitch: 67,
                    start_beat: 3.0,
                    length_beats: 0.5,
                    velocity: 100,
                    channel: 0,
                    expression: sphere_midi_service::NoteExpression::default(),
                    pitch_points: Vec::new(),
                    articulation: None,
                },
            ],
            controllers: Vec::new(),
            mpe: sphere_midi_service::mpe::MpeTrackConfiguration::default(),
        })],
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SAMPLE_RATE,
            buffer_size: FRAMES as u32,
        },
    };
    std::sync::Arc::make_mut(&mut snapshot.midi_clips[0]).length_beats = 4.0;

    let mut runtime =
        RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
            .expect("runtime builds");
    let envelope = render_bar_envelope(&mut runtime, 6.0, bpm);

    // Beat 0..1 is before the clip starts.
    let before = envelope[..4].iter().fold(0.0f32, |a, b| a.max(*b));
    assert!(before < 1.0e-6, "silent before the clip: {before}");

    // The clip's first note lands on beat 1, the second on beat 4.
    let first = envelope[4..6].iter().fold(0.0f32, |a, b| a.max(*b));
    let second = envelope[16..18].iter().fold(0.0f32, |a, b| a.max(*b));
    assert!(first > 0.001, "first note should sound: {first}");
    assert!(second > 0.001, "second note should sound: {second}");

    // The two-beat gap in the middle decays well below the note attacks.
    let gap = envelope[10..15].iter().fold(0.0f32, |a, b| a.max(*b));
    assert!(
        gap < first * 0.5,
        "gap between notes should decay: gap={gap} first={first}"
    );
}

#[test]
fn missing_font_leaves_the_track_silent_instead_of_failing_the_graph() {
    let font = FontFile::new("missing");
    let mut track = soundfont_track("sf-1", &font, test_font::MELODIC_PRESET);
    track.soundfont_path = Some("/definitely/not/a/soundfont.sf2".to_string());
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "soundfont-missing".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SAMPLE_RATE,
        tracks: vec![track, master_track()],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SAMPLE_RATE,
            buffer_size: FRAMES as u32,
        },
    };
    let mut runtime =
        RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
            .expect("graph still builds without the font");
    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    assert_eq!(render_peak(&mut runtime), 0.0);
}

/// Regression: a DSP Fx dropped on a Soundfont track used to be picked as
/// the track's "MIDI instrument" (any bridged insert on an Instrument track
/// qualified), so notes were pushed at the effect's bridge sink and the
/// player itself was starved — silence with an Fx, sound without one. The
/// player owns the notes; the effect only sees audio.
#[test]
fn an_effect_insert_does_not_take_the_notes_away_from_the_player() {
    let font = FontFile::new("effect-insert");
    let mut track = soundfont_track("sf-1", &font, test_font::MELODIC_PRESET);
    let mut params = HashMap::new();
    params.insert("format".to_string(), serde_json::json!("BuiltIn"));
    params.insert("classId".to_string(), serde_json::json!("equz8"));
    params.insert("role".to_string(), serde_json::json!("effect"));
    track.inserts.push(crate::types::EngineInsertSnapshot {
        id: "insert-sf-1-eq".to_string(),
        kind: "external-bridge-plugin".to_string(),
        enabled: true,
        params,
        state: None,
    });
    let mut runtime = build_runtime(vec![track, master_track()]);
    assert!(
        runtime.tracks[0].midi_instrument_insert_ix.is_none(),
        "an effect insert must not become the note destination"
    );
    runtime.midi_preview_note_on("sf-1", 0, 60, 100);
    assert!(
        render_peak(&mut runtime) > 0.001,
        "the player must still sound with an effect in the chain"
    );
}

// ---- MIDI tracks playing a Soundfont Multi (MIDI Out → Instrument) ----

fn midi_track(id: &str) -> EngineTrackSnapshot {
    let mut track = master_track();
    track.id = id.to_string();
    track.track_type = "midi".to_string();
    track
}

/// A Soundfont Multi on `sf-1` played by MIDI tracks `midi-a` (channel 1)
/// and `midi-b` (channel 2); only `midi-a` has a note, at beat 0.
fn routed_multi_runtime(font: &FontFile) -> RuntimeProject {
    let mut sf = soundfont_track("sf-1", font, test_font::MELODIC_PRESET);
    sf.soundfont_mode = sphere_soundfont_player::SoundfontPlayerMode::Multi;
    sf.soundfont_channels = melodic_parts();
    let clip = EngineMidiClipSnapshot {
        id: "clip-a".into(),
        track_id: "sf-1".into(),
        source_track_id: Some("midi-a".into()),
        start_beat: 0.0,
        length_beats: 4.0,
        notes: vec![EngineMidiNoteSnapshot {
            id: 1,
            pitch: 60,
            start_beat: 0.0,
            length_beats: 2.0,
            velocity: 100,
            channel: 0,
            expression: Default::default(),
            pitch_points: Vec::new(),
            articulation: None,
        }],
        controllers: Vec::new(),
        mpe: Default::default(),
    };
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "soundfont-routed".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: SAMPLE_RATE,
        tracks: vec![
            sf,
            midi_track("midi-a"),
            midi_track("midi-b"),
            master_track(),
        ],
        clips: Vec::new(),
        midi_clips: vec![std::sync::Arc::new(clip)],
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: SAMPLE_RATE,
            buffer_size: FRAMES as u32,
        },
    };
    RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
        .expect("routed runtime builds")
}

/// Schedules the first block of the timeline, then renders it.
fn play_first_block(runtime: &mut RuntimeProject) -> f32 {
    runtime.schedule_midi_block_with_offset(0, FRAMES as u64, 0);
    render_peak(runtime)
}

#[test]
fn a_routed_midi_track_plays_its_soundfont_multi_part() {
    let font = FontFile::new("routed-plays");
    let mut runtime = routed_multi_runtime(&font);
    assert_eq!(
        runtime.midi_tracks.len(),
        1,
        "routed notes join the instrument's schedule"
    );
    assert_eq!(runtime.midi_tracks[0].sources.len(), 1);
    assert!(play_first_block(&mut runtime) > 0.001);
}

#[test]
fn muting_a_routed_midi_track_silences_its_notes() {
    let font = FontFile::new("routed-mute");
    let mut runtime = routed_multi_runtime(&font);
    runtime.update_track_mute("midi-a", true);
    assert!(
        play_first_block(&mut runtime) < 1.0e-4,
        "the muted MIDI track must not sound"
    );
}

#[test]
fn soloing_a_routed_midi_track_keeps_its_instrument_audible() {
    let font = FontFile::new("routed-solo");
    let mut runtime = routed_multi_runtime(&font);
    runtime.update_track_solo("midi-a", true);
    assert!(
        play_first_block(&mut runtime) > 0.001,
        "solo on the MIDI track is a solo of what it plays"
    );
}

#[test]
fn soloing_another_midi_track_of_the_instrument_silences_this_one() {
    let font = FontFile::new("routed-solo-other");
    let mut runtime = routed_multi_runtime(&font);
    runtime.update_track_solo("midi-b", true);
    assert!(
        play_first_block(&mut runtime) < 1.0e-4,
        "only the soloed MIDI track plays"
    );
}

#[test]
fn soloing_the_instrument_keeps_every_midi_track_playing() {
    let font = FontFile::new("routed-solo-instrument");
    let mut runtime = routed_multi_runtime(&font);
    runtime.update_track_solo("midi-b", true);
    runtime.update_track_solo("sf-1", true);
    assert!(play_first_block(&mut runtime) > 0.001);
}
