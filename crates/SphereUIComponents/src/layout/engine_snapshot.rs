use crate::components::plugin_picker::STUB_PLUGIN_ID;
use crate::components::timeline::timeline_state::{
    self, vsti_output_bus_flat_range, vsti_output_bus_strip_indices,
    vsti_output_child_channels_for_bus_layout, vsti_output_child_track_id, ClipState, ClipType,
    InsertSlotState, MidiControllerKind, StretchMode, TimelineState, TrackState, TrackType,
    MASTER_TRACK_ID,
};

use DirectAudio::types::{
    EngineAutomationLaneSnapshot, EngineAutomationPointSnapshot, EngineAutomationTargetSnapshot,
    EngineClipAudioProcess, EngineClipSnapshot, EngineFadeSnapshot, EngineInsertSnapshot,
    EngineMidiClipSnapshot, EngineMidiControllerLane, EngineMidiControllerPoint,
    EngineMidiNoteSnapshot, EnginePitchPoint, EngineProjectSnapshot, EngineRoutingSnapshot,
    EngineSendSnapshot, EngineSolfegeSnapshot, EngineTempoPointSnapshot,
    EngineTrackInputSourceSnapshot, EngineTrackSnapshot, EngineWarpMarkerSnapshot,
};

/// Sampled-instrument articulation ids, matching
/// `solfege_model::voicebank`'s `ARTICULATION_*` constants.
///
/// Duplicated here rather than imported because `SphereUIComponents` does not
/// depend on `solfege-model`, and adding that dependency to carry four integers
/// would couple the whole UI crate to the container format. The
/// `articulation_ids_match_the_voicebank` test pins them.
mod voicebank_articulation {
    pub const SUSTAIN_VIBRATO: u16 = 0;
    pub const PIZZICATO: u16 = 1;
    pub const SPICCATO: u16 = 2;
    #[allow(dead_code)]
    pub const TREMOLO: u16 = 3;
}

/// Translate a score marking into the recorded articulation that plays it.
///
/// These are different alphabets. The editor's vocabulary is notation —
/// Sustain, Legato, Staccato, Tenuto, Accent, Marcato — and describes *how a
/// note is written*. A sampled bank's vocabulary is technique — sustain with
/// vibrato, spiccato, pizzicato, tremolo — and describes *how it was played*.
/// The mapping below is therefore a judgement, and a deliberately conservative
/// one: markings that mean "held" choose the sustained recording, markings that
/// mean "short and separated" choose the short bowed one.
///
/// Pizzicato and Tremolo are the exception, and they are not a judgement at
/// all: they name a playing technique directly, so they select the recording
/// of that technique. They were unreachable while the editor's vocabulary was
/// score markings only, which left 44% of a bank like Solo Violin — every
/// pizzicato and tremolo recording in it — impossible to select from the
/// arrangement no matter what the player wrote.
///
/// Returning `None` leaves the instrument on its own default, which is what
/// happened for every note before articulation reached the engine at all.
fn voicebank_articulation_for(articulation: timeline_state::ArticulationId) -> Option<u16> {
    use timeline_state::ArticulationId as A;
    match articulation {
        A::Sustain | A::Legato | A::Tenuto => Some(voicebank_articulation::SUSTAIN_VIBRATO),
        A::Staccato | A::Staccatissimo | A::Accent | A::Marcato => {
            Some(voicebank_articulation::SPICCATO)
        }
        A::Pizzicato => Some(voicebank_articulation::PIZZICATO),
        A::Tremolo => Some(voicebank_articulation::TREMOLO),
    }
}

/// Sampling step for a note's sounding-pitch trajectory, in seconds. The
/// engine smooths between the breakpoints it receives, so this only has to be
/// fine enough that a fast gesture is not aliased — 5 ms resolves a 100 Hz
/// wobble, well past anything a hand can draw or an instrument can articulate.
const PITCH_SAMPLE_SECONDS: f32 = 0.005;

/// Largest deviation, in cents, that the decimator is allowed to introduce by
/// dropping an intermediate breakpoint. One cent is roughly a fifth of the
/// smallest pitch difference a trained listener can hear on a sustained tone,
/// so a curve reconstructed within this tolerance is the curve that was drawn.
const PITCH_DECIMATE_CENTS: f32 = 1.0;

/// Emit the sounding-pitch trajectory of one note as engine breakpoints.
///
/// This is the hop that used to be missing: the editor stored a
/// [`PitchCurve`](timeline_state::PitchCurve) on the note and the trajectory
/// evaluator composed it with note transitions, but nothing ever carried the
/// result into the engine snapshot, so a drawn curve could never be heard.
///
/// Returns an empty vector when the note simply sounds at its notated pitch,
/// which keeps untouched projects byte-identical to before and lets the engine
/// skip the whole continuous-pitch path.
fn build_note_pitch_points(
    trajectory: &timeline_state::PitchTrajectory,
    notes: &[timeline_state::MidiNoteState],
    voice_of_note: &[usize],
    note_index: usize,
    playback_length_beats: f32,
    seconds_per_beat: f32,
) -> Vec<EnginePitchPoint> {
    let note = &notes[note_index];
    let voice = voice_of_note[note_index];

    let length = playback_length_beats.max(0.0);
    if length <= 0.0 {
        return Vec::new();
    }
    // Ask before sampling. A note with no drawn points that no transition
    // reaches sounds at the pitch it is written at, and the loop below would
    // spend a 5 ms-resolution pass over its whole length only to discover that
    // every sample equalled `note.pitch` and return nothing. Most notes in a
    // real arrangement are that note, and this runs on every project sync —
    // including every pointer event of a pitch drag.
    if !trajectory.note_departs_from_notated_pitch(notes, voice, note_index) {
        return Vec::new();
    }
    let step_beats = (PITCH_SAMPLE_SECONDS / seconds_per_beat.max(1e-6)).max(1e-4);
    let columns = ((length / step_beats).ceil() as usize).clamp(1, 1 << 16) + 1;

    let mut raw: Vec<Option<f32>> = Vec::with_capacity(columns);
    trajectory.sample_columns(notes, voice, note.start, step_beats, columns, &mut raw);

    // Fractional-semitone pitch per column, clamped to the note's own span so
    // the last point lands exactly on the note end rather than one step past.
    let notated = note.pitch as f32;
    let samples: Vec<(f32, f32)> = raw
        .iter()
        .enumerate()
        .map(|(column, value)| {
            (
                (step_beats * column as f32).min(length),
                value.unwrap_or(notated),
            )
        })
        .collect();

    // A note whose whole span sits within the tolerance of its notated pitch
    // has nothing to say; let the engine use the note number it already has.
    let tolerance = PITCH_DECIMATE_CENTS / 100.0;
    if samples
        .iter()
        .all(|(_, pitch)| (pitch - notated).abs() <= tolerance)
    {
        return Vec::new();
    }

    // Douglas–Peucker-style run decimation against the *reconstruction* the
    // engine will actually produce (a straight line between kept breakpoints),
    // so the emitted set reproduces the drawn curve to within `tolerance` at
    // every dropped column — not merely at the one adjacent to it. Checking a
    // single neighbour is the tempting shortcut and it silently flattens peaks
    // that fall in the middle of a long run.
    //
    // `MAX_RUN` bounds the inner rescan so this stays linear in practice; it
    // also guarantees a breakpoint at least every 320 ms, which keeps a voice
    // that joined late from waiting a whole note for its first pitch target.
    const MAX_RUN: usize = 64;
    let mut kept: Vec<(f32, f32)> = Vec::new();
    let mut anchor = 0usize;
    kept.push(samples[0]);
    for index in 1..samples.len() {
        let (anchor_beat, anchor_pitch) = samples[anchor];
        let (beat, pitch) = samples[index];
        let span = beat - anchor_beat;
        let fits = index - anchor <= MAX_RUN
            && samples[anchor + 1..index].iter().all(|(b, p)| {
                let t = if span > 1e-9 {
                    ((b - anchor_beat) / span).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                (anchor_pitch + (pitch - anchor_pitch) * t - p).abs() <= tolerance
            });
        if !fits {
            anchor = index - 1;
            kept.push(samples[anchor]);
        }
    }
    if kept.last().map(|(beat, _)| *beat) != Some(samples[samples.len() - 1].0) {
        kept.push(samples[samples.len() - 1]);
    }

    // A single point conveys no motion, and a run that decimated back to the
    // notated pitch is better expressed as "no points at all".
    if kept.len() < 2
        || kept
            .iter()
            .all(|(_, pitch)| (pitch - notated).abs() <= tolerance)
    {
        return Vec::new();
    }
    kept.into_iter()
        .map(|(beat, pitch)| EnginePitchPoint {
            beat: (note.start + beat).max(0.0) as f64,
            hz: timeline_state::midi_pitch_to_hz(pitch),
        })
        .collect()
}

/// Map every note index to the trajectory voice that owns it, so a note can
/// ask the trajectory for *its* line without re-deriving the voice partition.
fn voice_index_per_note(
    trajectory: &timeline_state::PitchTrajectory,
    note_count: usize,
) -> Vec<usize> {
    let mut owner = vec![0usize; note_count];
    for (voice, line) in trajectory.voices().iter().enumerate() {
        for &note in &line.notes {
            if note < note_count {
                owner[note] = voice;
            }
        }
    }
    owner
}

/// Per-channel `(start, pitch)` of every unmuted note in a clip, sorted by
/// start beat. Built once per clip at snapshot time so legato can find "the
/// next note on this channel" with a binary search instead of an O(n²) scan.
pub(crate) struct ArticulationLegatoIndex {
    /// `by_channel[ch]` = sorted `(start_beats, pitch)` for engine channel `ch`.
    by_channel: [Vec<(f32, u8)>; 16],
}

impl ArticulationLegatoIndex {
    pub(crate) fn build(
        notes: &[timeline_state::MidiNoteState],
        output_mode: timeline_state::MidiOutputChannelMode,
    ) -> Self {
        let mut by_channel: [Vec<(f32, u8)>; 16] = Default::default();
        for note in notes.iter().filter(|n| !n.muted) {
            let channel = output_mode.resolve(note.channel).raw().min(15) as usize;
            by_channel[channel].push((note.start.max(0.0), note.pitch.min(127)));
        }
        for list in &mut by_channel {
            list.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        Self { by_channel }
    }

    /// The first note on `channel` starting strictly after `start` (chord
    /// members at the same beat are not "next"). Returns `(start, pitch)`.
    fn next_note_after(&self, channel: u8, start: f32) -> Option<(f32, u8)> {
        const EPS: f32 = 1.0e-4;
        let list = &self.by_channel[channel.min(15) as usize];
        let idx = list.partition_point(|(s, _)| *s <= start + EPS);
        list.get(idx).copied()
    }
}

/// Resolve and apply a note's articulation for scheduling: per-note wins,
/// otherwise the clip's direction lane is chased at the note start. Returns
/// the `(length_beats, velocity)` to schedule; the stored note is untouched.
/// Legato onto the *same* pitch clamps the gate to exactly the next note's
/// start (no overlap): the runtime sorts NoteOff before NoteOn at the same
/// sample, so the retrigger stays clean instead of the off killing the new
/// voice.
pub(crate) fn articulated_note_playback(
    note: &timeline_state::MidiNoteState,
    articulations: &[timeline_state::MidiArticulationEvent],
    channel: u8,
    legato_index: &ArticulationLegatoIndex,
) -> (f32, u8) {
    let velocity = note.velocity.clamp(1, 127);
    let Some(articulation) = timeline_state::resolve_note_articulation(note, articulations) else {
        return (note.duration, velocity);
    };
    let next = legato_index.next_note_after(channel, note.start.max(0.0));
    let (mut length, velocity) = timeline_state::apply_articulation_playback(
        note.start.max(0.0),
        note.duration,
        velocity,
        articulation,
        next.map(|(start, _)| start),
    );
    if let Some((next_start, next_pitch)) = next {
        if next_pitch == note.pitch.min(127) {
            // Same-pitch neighbor: never let the gate cross its NoteOn.
            let to_next = (next_start - note.start.max(0.0)).max(timeline_state::MIN_NOTE_BEATS);
            length = length.min(to_next);
        }
    }
    (length, velocity)
}

/// Canonical engine `mode` key for a clip's stretch mode (matches
/// `runtime::resolve_clip_processor`).
fn stretch_mode_key(mode: StretchMode) -> &'static str {
    match mode {
        StretchMode::Off => "off",
        StretchMode::Resample => "resample",
        StretchMode::TempoSync => "temposync",
        StretchMode::Manual => "manual",
        StretchMode::Warp => "warp",
    }
}

fn sphere_stretch_params_from_clip_stretch(
    stretch: &timeline_state::AudioClipStretchState,
    project_bpm: f64,
) -> SphereAudioProcessor::StretchParams {
    stretch.to_sphere_stretch_params(project_bpm)
}

/// The `(asset id, media path)` a clip renders audio from, or `None` when it
/// has no audio source.
///
/// A Video clip resolves here too: the engine decodes the audio stream of the
/// container (`mp4`/`m4v`/`mov`), so a reference video plays its own sound
/// through the Video track's channel rather than running silent against
/// picture. Containers with no audio track simply fail to load and are skipped
/// by the engine, exactly like an unreadable audio file.
fn clip_media_source(clip: &ClipState) -> Option<(&String, &String)> {
    let (file_id, source_path) = match &clip.clip_type {
        ClipType::Audio {
            file_id,
            source_path: Some(source_path),
        }
        | ClipType::Video {
            file_id,
            source_path: Some(source_path),
        } => (file_id, source_path),
        _ => return None,
    };
    (!source_path.trim().is_empty()).then_some((file_id, source_path))
}

fn is_renderable_audio_clip(clip: &ClipState) -> bool {
    !clip.muted && clip_media_source(clip).is_some()
}

fn clip_source_offset_seconds(state: &TimelineState, clip: &ClipState) -> f64 {
    let stretch = &clip.stretch;
    if stretch.source_start_samples > 0 {
        let source_rate = stretch
            .original_sample_rate
            .max(stretch.project_sample_rate)
            .max(1) as f64;
        stretch.source_start_samples as f64 / source_rate
    } else {
        // Keep legacy projects whose trims were stored only as beat offsets
        // audible at the same source position.
        state.beats_to_seconds(clip.offset_beats.max(0.0)) as f64
    }
}

fn apply_auto_crossfades(state: &TimelineState, clips: &mut [EngineClipSnapshot]) {
    for track in &state.tracks {
        let mut track_audio: Vec<&ClipState> = track
            .clips
            .iter()
            .filter(|clip| is_renderable_audio_clip(clip))
            .collect();
        track_audio.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));

        for pair in track_audio.windows(2) {
            let a = pair[0];
            let b = pair[1];
            let a_end = a.start_beat + a.duration_beats;
            let b_end = b.start_beat + b.duration_beats;
            let overlap_start = a.start_beat.max(b.start_beat);
            let overlap_end = a_end.min(b_end);
            if overlap_end <= overlap_start {
                continue;
            }

            let overlap_beats = overlap_end - overlap_start;
            let overlap_seconds = state.beats_to_seconds(overlap_beats).max(0.0) as f64;
            if overlap_seconds <= 0.0 {
                continue;
            }
            extend_engine_fade(clips, &a.id, 0.0, overlap_seconds);
            extend_engine_fade(clips, &b.id, overlap_seconds, 0.0);
        }
    }
}

fn extend_engine_fade(
    clips: &mut [EngineClipSnapshot],
    clip_id: &str,
    fade_in_seconds: f64,
    fade_out_seconds: f64,
) {
    let Some(clip) = clips.iter_mut().find(|clip| clip.id == clip_id) else {
        return;
    };
    let fades = clip.fades.get_or_insert_with(|| EngineFadeSnapshot {
        in_duration: 0.0,
        out_duration: 0.0,
        in_curve: "equal_power".to_string(),
        out_curve: "equal_power".to_string(),
    });
    fades.in_duration = fades.in_duration.max(fade_in_seconds);
    fades.out_duration = fades.out_duration.max(fade_out_seconds);
    fades.in_curve = "equal_power".to_string();
    fades.out_curve = "equal_power".to_string();
}

/// Map a controller lane kind to its VST3 controller number, or `None` for
/// kinds with no global controller mapping (poly pressure is per-note and not
/// yet routed to the engine).
fn vst3_controller_number(kind: MidiControllerKind) -> Option<u16> {
    match kind {
        MidiControllerKind::CC(n) => Some(n as u16),
        MidiControllerKind::ChannelPressure => Some(128), // kAfterTouch
        MidiControllerKind::PitchBend => Some(129),       // kPitchBend
        MidiControllerKind::PolyPressure => None,
    }
}

/// Resolve a track's audio input to concrete device + channels for the engine.
///
/// The track stores only an `AudioConnectionId`; the registry is the single
/// layer that knows physical ports. A connection that is missing, disabled, or
/// unresolvable yields an empty source — recording silence — rather than a
/// fallback to an unrelated input.
///
/// MIDI never passes through here: it stays on `routing.midi_input`.
pub(crate) fn build_engine_input_source(
    track: &TrackState,
    connections: &crate::audio_connections::AudioConnectionRegistry,
    ports: &crate::audio_connections::AvailablePorts,
) -> EngineTrackInputSourceSnapshot {
    let empty = || EngineTrackInputSourceSnapshot {
        device_id: None,
        channels: Vec::new(),
    };
    // No Input is never reinterpreted as hardware channels 1-2: recording must
    // fail clearly instead of capturing an unintended source.
    let Some(connection_id) = track.routing.audio_input_connection_id.as_ref() else {
        return empty();
    };
    let Some(connection) = connections.get(connection_id) else {
        return empty();
    };
    match connections.resolved_ports(connection_id, ports) {
        Some(channels) => EngineTrackInputSourceSnapshot {
            device_id: connection.device_id.clone(),
            channels,
        },
        None => empty(),
    }
}

pub(crate) fn apply_engine_track_input_state(
    engine: &DirectAudio::native::AudioEngine,
    track: &TrackState,
    connections: &crate::audio_connections::AudioConnectionRegistry,
) -> Result<(), String> {
    let ports = crate::audio_connections::current_available_ports();
    engine
        .update_track_input_state(
            &track.id,
            track.armed,
            track.input_monitor.is_active(track.armed),
            build_engine_input_source(track, connections, &ports),
        )
        .map_err(|error| error.to_string())
}

/// Build the DirectAudio insert descriptors for one track's mixer insert chain
/// (Phase 2b). Only real, instantiable VST3 plugins are emitted as
/// `native-plugin` descriptors — DirectAudio then instantiates a
/// `Vst3RuntimeProcessor` on its worker and routes audio through it. The
/// documented stub (`STUB_PLUGIN_ID`) and any slot without a usable path are
/// skipped so the realtime runtime keeps no-op'ing on placeholders rather than
/// logging passthrough noise.
///
/// `enabled` mirrors the UI bypass flag (`!bypassed`), so toggling bypass in
/// the mixer changes the audio path on the next engine sync. This runs on the
/// UI thread inside snapshot construction — never the audio callback.
fn log_track_insert_chain(track_id: &str, inserts: &[EngineInsertSnapshot]) {
    if inserts.is_empty() || !crate::perf::engine_sync_debug_enabled() {
        return;
    }
    let chain: Vec<String> = inserts
        .iter()
        .enumerate()
        .map(|(i, ins)| format!("slot{i}:{}", ins.id))
        .collect();
    eprintln!(
        "[GraphBuild] track={track_id} inserts=[{}] runtime_insert_count={}",
        chain.join(", "),
        inserts.len()
    );
}

/// Whether `track_type` can host an instrument at all.
///
/// Only an Instrument or MIDI track carries note events. Everything else —
/// audio, bus, return, group, master, video — carries audio and nothing but.
fn track_type_hosts_instrument(track_type: TrackType) -> bool {
    matches!(track_type, TrackType::Instrument | TrackType::Midi)
}

/// `params["role"]` for a bridged insert, which the engine turns into
/// `RuntimeInsert::bridge_is_effect`.
///
/// The role is not cosmetic — it selects two different bridge behaviours in
/// `engine::render::apply_external_bridge_insert_block`:
///
/// * `"effect"` writes the track's dry block into the plugin's shared input
///   region and **replaces** the block with the returned wet block;
/// * `"instrument"` writes no input at all and **adds** the plugin's output on
///   top of the dry signal.
///
/// So a plug-in that mis-declares itself on an audio track is not merely
/// mislabelled: it is fed silence and its output is summed over the untouched
/// dry signal, which is indistinguishable from the insert not being there. A
/// VST3 whose `subCategories` lead with `Instrument` while it is in fact an EQ
/// does exactly this, and the scanner has no better source than what the module
/// declares about itself (`SpherePluginHost::registry::classify_kind`).
///
/// The track type is the authority the declaration is not: an audio/bus/return/
/// master track has no note source, so an insert on one can only ever run as an
/// effect regardless of what it claims. On a track that *can* host an instrument
/// the declaration is trusted as before.
///
/// `builtin_instrument` says the track's note source is the built-in Soundfont
/// Player or the native Solfege engine. Those are track instruments, not
/// inserts, so every insert on such a track is an effect *by construction*:
/// the legacy slot-zero fallback would otherwise turn the first DSP Fx a user
/// drops on a Soundfont track into an "instrument" that is fed no audio and
/// merely adds its (silent) output — the Fx appears to do nothing.
fn bridge_insert_role(
    track_type: TrackType,
    slot_index: usize,
    plugin_is_instrument: Option<bool>,
    builtin_instrument: bool,
) -> &'static str {
    if !track_type_hosts_instrument(track_type) || builtin_instrument {
        return "effect";
    }
    match plugin_is_instrument {
        Some(true) => "instrument",
        Some(false) => "effect",
        None if slot_index == 0 => "instrument",
        None => "effect",
    }
}

fn build_engine_inserts_for(
    track_id: &str,
    track_type: TrackType,
    slots: &[InsertSlotState],
    export_mode: bool,
    builtin_instrument: bool,
) -> Vec<EngineInsertSnapshot> {
    use crate::components::timeline::timeline_state::InsertPluginFormat;

    // When the external bridge is active, live inserts (VST3 + built-in) are
    // `external-bridge-plugin` descriptors processed through realtime bridge
    // sinks. Offline export detaches those sinks and drives them from the
    // export worker (`export_window::BridgeSinkHandoff`), so the export snapshot
    // must keep the same insert kinds — not rebuild as in-process `native-plugin`
    // inserts (which would bypass the live DSP and render dry/raw audio).
    if super::plugin_bridge_runtime::bridge_enabled() {
        return slots
            .iter()
            .enumerate()
            .filter_map(|(slot_index, slot)| {
                let plugin_id = slot.plugin_id.as_deref()?;
                if plugin_id == STUB_PLUGIN_ID {
                    return None;
                }
                let is_builtin = SpherePluginHost::builtin_audio_bridge_supported(plugin_id);
                let is_audio_unit =
                    !is_builtin && slot.plugin_format == Some(InsertPluginFormat::Au);
                // VST3, VST2 and CLAP are the module formats the native bridge
                // can instantiate; all three travel the same external-bridge
                // path.
                let is_module_plugin = matches!(
                    slot.plugin_format,
                    Some(
                        InsertPluginFormat::Vst3
                            | InsertPluginFormat::Vst2
                            | InsertPluginFormat::Clap
                    )
                );
                if !is_builtin && !is_audio_unit && !is_module_plugin {
                    return None;
                }
                // Neither a built-in nor an Audio Unit has a module path; the AU
                // component id travels in `classId` like any other identity.
                let path = if is_builtin || is_audio_unit {
                    String::new()
                } else {
                    slot.plugin_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .filter(|p| !p.trim().is_empty())?
                };

                let role = bridge_insert_role(
                    track_type,
                    slot_index,
                    slot.plugin_is_instrument,
                    builtin_instrument,
                );

                let mut params: std::collections::HashMap<String, serde_json::Value> =
                    std::collections::HashMap::new();
                // The engine distinguishes built-in parameter handling from
                // bridged plug-ins by this field, so it has to name the real
                // format rather than call everything external VST3.
                params.insert(
                    "format".to_string(),
                    serde_json::json!(if is_builtin {
                        "BuiltIn"
                    } else if is_audio_unit {
                        "AU"
                    } else if slot.plugin_format == Some(InsertPluginFormat::Vst2) {
                        "VST2"
                    } else if slot.plugin_format == Some(InsertPluginFormat::Clap) {
                        "CLAP"
                    } else {
                        "VST3"
                    }),
                );
                params.insert("modulePath".to_string(), serde_json::json!(path));
                params.insert("path".to_string(), serde_json::json!(path));
                params.insert("classId".to_string(), serde_json::json!(plugin_id));
                params.insert("class_id".to_string(), serde_json::json!(plugin_id));
                params.insert("pluginInstanceId".to_string(), serde_json::json!(slot.id));
                params.insert(
                    "displayName".to_string(),
                    serde_json::json!(slot.display_name),
                );
                params.insert(
                    "enabledAudioOutputChannels".to_string(),
                    serde_json::json!(normalized_enabled_audio_outputs(slot)),
                );
                params.insert(
                    "vstiOutputChildren".to_string(),
                    vsti_output_children_json(slot),
                );
                params.insert("bridge".to_string(), serde_json::json!(true));
                params.insert("role".to_string(), serde_json::json!(role));

                if crate::perf::engine_sync_debug_enabled() {
                    eprintln!(
                        "[GraphBuild] track={} insert={} instance={} kind=external-bridge-plugin",
                        track_id, slot.id, slot.id
                    );
                }

                Some(EngineInsertSnapshot {
                    id: slot.id.clone(),
                    kind: "external-bridge-plugin".to_string(),
                    enabled: slot.enabled && !slot.bypassed,
                    params,
                    state: None,
                })
            })
            .collect();
    }

    slots
        .iter()
        .filter_map(|slot| {
            let plugin_id = slot.plugin_id.as_deref()?;
            // Skip the placeholder stub — it has no real processor.
            if plugin_id == STUB_PLUGIN_ID {
                return None;
            }
            // Only a module format with a real path is instantiable in-process;
            // AU and the built-ins take other routes.
            let format_label = match slot.plugin_format {
                Some(InsertPluginFormat::Vst3) => "VST3",
                Some(InsertPluginFormat::Vst2) => "VST2",
                Some(InsertPluginFormat::Clap) => "CLAP",
                _ => return None,
            };
            let path = slot
                .plugin_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|p| !p.trim().is_empty())?;

            let mut params: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            params.insert("format".to_string(), serde_json::json!(format_label));
            params.insert("modulePath".to_string(), serde_json::json!(path));
            params.insert("path".to_string(), serde_json::json!(path));
            params.insert("classId".to_string(), serde_json::json!(plugin_id));
            params.insert("class_id".to_string(), serde_json::json!(plugin_id));
            params.insert("pluginInstanceId".to_string(), serde_json::json!(slot.id));
            params.insert(
                "displayName".to_string(),
                serde_json::json!(slot.display_name),
            );
            params.insert(
                "enabledAudioOutputChannels".to_string(),
                serde_json::json!(normalized_enabled_audio_outputs(slot)),
            );
            params.insert(
                "vstiOutputChildren".to_string(),
                vsti_output_children_json(slot),
            );

            Some(EngineInsertSnapshot {
                id: slot.id.clone(),
                kind: "native-plugin".to_string(),
                enabled: slot.enabled && !slot.bypassed,
                params,
                // Carry the saved VST3 state into the offline graph so the
                // freshly-instantiated in-process processor renders with the
                // user's current tweaks. Live in-process builds keep `None`
                // (their state is restored through the existing engine path).
                state: if export_mode {
                    slot.vst3_state.as_ref().map(|a| a.as_ref().clone())
                } else {
                    None
                },
            })
        })
        .collect()
}

fn normalized_enabled_audio_outputs(slot: &InsertSlotState) -> Vec<u8> {
    let mut channels = if slot.enabled_audio_output_channels.is_empty() {
        vec![1, 2]
    } else {
        slot.enabled_audio_output_channels.clone()
    };
    if !channels.contains(&1) {
        channels.push(1);
    }
    if !channels.contains(&2) {
        channels.push(2);
    }
    channels.retain(|channel| (1..=32).contains(channel));
    channels.sort_unstable();
    channels.dedup();
    channels
}

fn vsti_output_children_json(slot: &InsertSlotState) -> serde_json::Value {
    let bus_counts = &slot.output_bus_channel_counts;
    // Mirror `ensure_vsti_output_child_tracks` exactly so child track ids line up:
    // child routes are created only from declared multi-output capability data.
    let bus_indices = vsti_output_bus_strip_indices(bus_counts);
    serde_json::Value::Array(
        bus_indices
            .into_iter()
            .filter_map(|bus_index| {
                // Real flat-channel pair for this bus. Mono bus → (ch, ch) so the
                // engine duplicates it to L/R; stereo → (l, r) preserved.
                let (channel_l, channel_r) =
                    vsti_output_child_channels_for_bus_layout(bus_counts, bus_index)?;
                let channel_count = if bus_counts.len() == 1 && bus_counts[0] > 2 {
                    if channel_l == channel_r {
                        1
                    } else {
                        2
                    }
                } else if bus_counts.is_empty() {
                    2
                } else {
                    vsti_output_bus_flat_range(bus_counts, bus_index as usize)
                        .map(|(_, count)| count)
                        .unwrap_or(2)
                };
                let child_id = vsti_output_child_track_id(&slot.id, bus_index);
                Some(serde_json::json!({
                    "trackId": child_id,
                    "pluginInstanceId": slot.id,
                    "busIndex": bus_index,
                    "channelCount": channel_count,
                    "channelL": channel_l,
                    "channelR": channel_r,
                    "mixerChannelId": child_id,
                    "routeNodeId": child_id,
                }))
            })
            .collect(),
    )
}

fn build_engine_inserts(track: &TrackState, export_mode: bool) -> Vec<EngineInsertSnapshot> {
    build_engine_inserts_for(
        &track.id,
        track.track_type,
        &track.inserts,
        export_mode,
        track_has_builtin_instrument(track),
    )
}

/// The track sounds through a built-in instrument (Soundfont Player or Solfege)
/// rather than a hosted VSTi insert.
fn track_has_builtin_instrument(track: &TrackState) -> bool {
    track.builtin_soundfont_player || track.solfege.is_some()
}

/// Build the DirectAudio send descriptors for one track (Phase 3). Each send carries
/// a linear level (from `gain_db`) and its target Bus/Return track id; DirectAudio
/// accumulates the scaled signal into the target's receive buffer. Sends with
/// no target are skipped. Pre-fader is persisted but the runtime currently taps
/// post-fader only. Runs on the UI thread during snapshot construction.
fn build_engine_sends(track: &TrackState) -> Vec<EngineSendSnapshot> {
    track
        .sends
        .iter()
        .filter(|s| !s.target_track_id.trim().is_empty())
        .map(|s| EngineSendSnapshot {
            id: s.id.clone(),
            return_track_id: s.target_track_id.clone(),
            level: s.gain_linear(),
            enabled: s.enabled,
            pre_fader: s.pre_fader,
        })
        .collect()
}

fn build_engine_automation_lanes(track: &TrackState) -> Vec<EngineAutomationLaneSnapshot> {
    track
        .automation_lanes
        .iter()
        .map(|lane| {
            let mut target = EngineAutomationTargetSnapshot {
                tag: lane.target.to_tag(),
                ..Default::default()
            };
            match &lane.target {
                timeline_state::AutomationTarget::PluginParameter {
                    insert_id,
                    parameter_id,
                    parameter_name,
                } => {
                    target.insert_id = insert_id.clone();
                    target.parameter_id = parameter_id.clone();
                    target.parameter_name = parameter_name.clone();
                }
                timeline_state::AutomationTarget::SendLevel { send_id } => {
                    target.send_id = send_id.clone();
                }
                _ => {}
            }

            // Track Volume automation also honors the per-track `automation read`
            // toggle: when read is off the runtime must fall back to base volume,
            // so we disable the lane in the snapshot. The runtime stays a pure
            // value copy — it never reads UI state — and base volume is always
            // sent as `EngineTrackSnapshot.volume`, so this never double-applies.
            let enabled = match lane.target {
                timeline_state::AutomationTarget::TrackVolume => {
                    lane.enabled && track.volume_automation_read
                }
                _ => lane.enabled,
            };

            EngineAutomationLaneSnapshot {
                id: lane.id.clone(),
                name: lane.name.clone(),
                target,
                enabled,
                points: lane
                    .points
                    .iter()
                    .map(|point| EngineAutomationPointSnapshot {
                        beat: point.beat.max(0.0) as f64,
                        value: point.value.clamp(0.0, 1.0),
                        curve: point.curve.to_tag(),
                        tension: point.tension.clamp(-1.0, 1.0),
                    })
                    .collect(),
            }
        })
        .collect()
}

/// Live-path snapshot: plugin inserts follow the configured backend (bridged by
/// default). Used by every realtime engine sync.
pub(crate) fn build_engine_project_snapshot(
    state: &TimelineState,
    sample_rate: u32,
    project_root: Option<&str>,
    preferred_input_device: Option<&str>,
) -> EngineProjectSnapshot {
    // Live path: realtime PDC is governed by the engine's own atomic, so the
    // snapshot's `pdc_enabled` is unused here — default it to the engine default.
    build_engine_project_snapshot_inner(
        state,
        sample_rate,
        project_root,
        preferred_input_device,
        false,
        true,
        0,
    )
}

/// Offline-export snapshot: when the external bridge is active, inserts mirror
/// the live bridge graph so export can drive the detached realtime bridge sinks.
/// Legacy in-process export (bridge off) carries saved VST3 state instead.
/// `pdc_enabled` / `latency_graph_version` are stamped from the live engine so the
/// offline render uses the *same* latency-compensated graph as playback.
pub(super) fn build_engine_project_snapshot_for_export(
    state: &TimelineState,
    sample_rate: u32,
    project_root: Option<&str>,
    preferred_input_device: Option<&str>,
    pdc_enabled: bool,
    latency_graph_version: u64,
) -> EngineProjectSnapshot {
    build_engine_project_snapshot_inner(
        state,
        sample_rate,
        project_root,
        preferred_input_device,
        true,
        pdc_enabled,
        latency_graph_version,
    )
}

fn build_engine_project_snapshot_inner(
    state: &TimelineState,
    sample_rate: u32,
    project_root: Option<&str>,
    preferred_input_device: Option<&str>,
    export_mode: bool,
    pdc_enabled: bool,
    latency_graph_version: u64,
) -> EngineProjectSnapshot {
    let engine_ports = crate::audio_connections::current_available_ports();
    let mut tracks: Vec<EngineTrackSnapshot> = state
        .tracks
        .iter()
        .map(|track| EngineTrackSnapshot {
            id: track.id.clone(),
            track_type: track_type_name(track.track_type).to_string(),
            // The value under the user's finger, not the one they moved away
            // from. A fader drag holds its new value in the preview map and
            // only writes `track.volume` on release, so a sync landing mid-drag
            // published the *old* volume and overwrote the live param push that
            // had already made the track quieter — the fader would jump back to
            // being loud until the user let go.
            volume: volume_norm_to_linear(state.display_track_volume(track)),
            pan: track.pan.clamp(-1.0, 1.0),
            muted: track.muted,
            solo: track.solo,
            armed: track.armed,
            input_monitor: track.input_monitor.is_active(track.armed),
            input_source: build_engine_input_source(track, &state.audio_connections, &engine_ports),
            // Track audio format controls input/recording channel selection.
            // Engine output remains stereo so mono-input tracks still route to
            // the stereo master/bus instead of collapsing the playback graph.
            preview_mode: "stereo".to_string(),
            output_track_id: match &track.routing.output {
                timeline_state::TrackOutputRouting::Bus { bus_id } => Some(bus_id.clone()),
                timeline_state::TrackOutputRouting::Main
                | timeline_state::TrackOutputRouting::None
                | timeline_state::TrackOutputRouting::HardwareOutput { .. }
                // Instrument-routing redirects MIDI events (see `midi_clips`
                // below), not audio bus summing — a MIDI track has no audio
                // of its own to route.
                | timeline_state::TrackOutputRouting::Instrument { .. } => None,
            },
            inserts: {
                let inserts = build_engine_inserts(track, export_mode);
                log_track_insert_chain(&track.id, &inserts);
                inserts
            },
            sends: build_engine_sends(track),
            automation_lanes: build_engine_automation_lanes(track),
            builtin_soundfont_player: track.builtin_soundfont_player,
            soundfont_path: track.soundfont_path.clone(),
            soundfont_preset_bank: track.soundfont_preset.map(|(bank, _)| bank),
            soundfont_preset_patch: track.soundfont_preset.map(|(_, patch)| patch),
            soundfont_volume: track.soundfont_volume,
            soundfont_reverb_chorus: track.soundfont_reverb_chorus,
            soundfont_polyphony: track.soundfont_polyphony,
            soundfont_envelope: track.soundfont_envelope,
            soundfont_quality: track.soundfont_quality,
            solfege_engine: track.solfege.as_ref().map(|state| EngineSolfegeSnapshot {
                model_path: state.model_path.clone(),
                instrument: state.instrument.clone(),
                voice: state.voice.clone(),
                preset: state.preset.clone(),
                bow_pressure: state.bow_pressure,
                vibrato: state.vibrato,
                dynamics: state.dynamics,
                expression: state.expression,
            }),
        })
        .collect();

    let master_inserts = build_engine_inserts_for(
        MASTER_TRACK_ID,
        TrackType::Master,
        &state.master.inserts,
        export_mode,
        false,
    );
    log_track_insert_chain(MASTER_TRACK_ID, &master_inserts);

    tracks.push(EngineTrackSnapshot {
        id: "master".to_string(),
        track_type: "master".to_string(),
        volume: volume_norm_to_linear(state.display_master_volume()),
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
        inserts: master_inserts,
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
        solfege_engine: None,
    });

    let mut clips: Vec<EngineClipSnapshot> = state
        .tracks
        .iter()
        .flat_map(|track| {
            track.clips.iter().filter_map(move |clip| {
                if clip.muted {
                    return None;
                }
                let (file_id, source_path) = clip_media_source(clip)?;

                // Resolve the non-destructive stretch/pitch state into the
                // engine's render parameters. `speed_ratio` folds time-stretch
                // and pitch (see `AudioClipStretchState::resample_speed_ratio`);
                // the clip's `duration_beats` already reflects the ratio (the
                // inspector couples length to ratio), so playback and export —
                // which share `render_project_block_interleaved` — stay in sync.
                //
                // `ClipState::gain` and track pan remain the canonical gain/pan
                // sources for this slice. The stored stretch gain/pan fields are
                // intentionally not applied here so the engine cannot double-gain
                // a clip before the inspector is migrated to a single process
                // state.
                let stretch = &clip.stretch;
                let project_bpm = state.bpm.max(1.0) as f64;
                let sphere_stretch = sphere_stretch_params_from_clip_stretch(stretch, project_bpm);
                let effective_time_ratio = SphereAudioProcessor::effective_time_ratio(
                    &sphere_stretch,
                    Some(project_bpm as f32),
                ) as f64;
                let pitch_ratio = timeline_state::AudioClipStretchState::pitch_ratio_from_semitones(
                    stretch.pitch_shift_semitones,
                );
                let preserve_pitch =
                    matches!(stretch.mode, StretchMode::Manual | StretchMode::TempoSync)
                        && stretch.preserve_pitch;
                let fades = if stretch.fade_in_ms > 0.0 || stretch.fade_out_ms > 0.0 {
                    Some(EngineFadeSnapshot {
                        in_duration: (stretch.fade_in_ms.max(0.0) as f64) / 1000.0,
                        out_duration: (stretch.fade_out_ms.max(0.0) as f64) / 1000.0,
                        in_curve: "equal_power".to_string(),
                        out_curve: "equal_power".to_string(),
                    })
                } else {
                    None
                };

                Some(EngineClipSnapshot {
                    id: clip.id.clone(),
                    track_id: track.id.clone(),
                    asset_id: file_id.clone(),
                    media_path: Some(source_path.clone()),
                    start_beat: clip.start_beat.max(0.0) as f64,
                    duration_beats: clip.duration_beats.max(0.0) as f64,
                    offset_seconds: clip_source_offset_seconds(state, clip),
                    gain: clip.gain.clamp(0.0, 4.0),
                    muted: clip.muted,
                    // On an ARA track the plug-in renders every audio clip,
                    // having read the sources out of band; the engine must skip
                    // their files or each clip would play twice.
                    ara_rendered: track.ara.is_some(),
                    fades,
                    stretch: sphere_stretch.clone(),
                    audio_process: Some(EngineClipAudioProcess {
                        speed_ratio: SphereAudioProcessor::source_read_rate_for_repitch(
                            &sphere_stretch,
                            Some(project_bpm as f32),
                        ) as f64,
                        effective_time_ratio,
                        pitch_ratio,
                        pitch_semitones: stretch.pitch_shift_semitones as f64,
                        preserve_pitch,
                        mode: stretch_mode_key(stretch.mode).to_string(),
                        quality: stretch.algorithm.label().to_string(),
                        source_start_samples: stretch.source_start_samples,
                        source_end_samples: stretch.source_end_samples,
                        warp_markers: {
                            let mut markers: Vec<_> = stretch
                                .warp_markers
                                .iter()
                                .map(|marker| EngineWarpMarkerSnapshot {
                                    id: marker.id,
                                    source_sample: marker.source_sample,
                                    timeline_beat: marker.timeline_beat,
                                    locked: marker.locked,
                                })
                                .collect();
                            markers.sort_by(|a, b| a.timeline_beat.total_cmp(&b.timeline_beat));
                            markers
                        },
                        reverse: stretch.reverse,
                        denoise_amount: stretch.denoise_amount.clamp(0.0, 1.0),
                        channel_transform: stretch.channel_transform,
                        dc_remove: stretch.dc_remove,
                        dc_left: stretch.dc_left,
                        dc_right: stretch.dc_right,
                        extra_gain: 1.0,
                        dehum_hz: stretch.dehum_hz,
                        dehum_harmonics: stretch.dehum_harmonics,
                        dehum_reduction_db: stretch.dehum_reduction_db,
                        envelope_points: stretch
                            .gain_envelope
                            .points
                            .iter()
                            .map(|p| (p.time, p.value_db))
                            .collect(),
                        preview_bypass: false,
                    }),
                })
            })
        })
        .collect();
    apply_auto_crossfades(state, &mut clips);

    // MIDI clips (Phase 2): notes stay clip-relative; the engine resolves them
    // to absolute beats/samples. Muted clips are skipped, matching audio clips.
    let midi_clips = state
        .tracks
        .iter()
        .flat_map(|track| {
            // A MIDI track with no Instrument plugin of its own can route its
            // notes to an Instrument track's plugin instead
            // (`TrackOutputRouting::Instrument`); everything else (including
            // an Instrument track's own clips) keeps playing through its own
            // track id, unchanged.
            let track_id = state
                .effective_instrument_track_id(&track.id)
                .unwrap_or_else(|| track.id.clone());
            track.clips.iter().filter_map(move |clip| {
                if clip.muted {
                    return None;
                }
                let ClipType::Midi {
                    notes,
                    controller_lanes,
                    articulations,
                    ..
                } = &clip.clip_type
                else {
                    return None;
                };
                // Fixed-channel tracks force every event onto one channel
                // (the pre-existing behavior); PerNote tracks emit each
                // note's own channel and controller lanes still ride the
                // track's fixed/default channel (per-channel CC lanes are a
                // follow-up, not part of this pass).
                let output_mode = track.routing.output_channel_mode();
                let lane_channel = output_mode
                    .resolve(track.routing.default_note_channel())
                    .raw();
                // Articulations are applied here — and only here — so realtime
                // playback and offline export (same builder) stay equivalent
                // and the stored note data is never rewritten. Direction
                // chasing is a pure beat lookup over the clip's event list, so
                // it is independent of where the transport starts/seeks/loops.
                let legato_index = ArticulationLegatoIndex::build(notes, output_mode);
                // The evaluated pitch trajectory is built from the *same*
                // notes and articulation events the piano roll and the Pitch
                // editor render, so what is drawn, what is displayed and what
                // is played are one evaluation, not three.
                let trajectory =
                    timeline_state::PitchTrajectory::build(notes, articulations.as_slice());
                let voice_of_note = voice_index_per_note(&trajectory, notes.len());
                let seconds_per_beat = 60.0 / state.bpm.max(1.0);
                Some(EngineMidiClipSnapshot {
                    id: clip.id.clone(),
                    track_id: track_id.clone(),
                    start_beat: clip.start_beat.max(0.0) as f64,
                    length_beats: clip.duration_beats.max(0.0) as f64,
                    notes: notes
                        .iter()
                        .enumerate()
                        // Muted notes stay in the clip but emit no runtime event.
                        .filter(|(_, n)| !n.muted)
                        .map(|(index, n)| {
                            let channel = output_mode.resolve(n.channel).raw();
                            let (length_beats, velocity) =
                                articulated_note_playback(n, articulations, channel, &legato_index);
                            EngineMidiNoteSnapshot {
                                id: n.id,
                                pitch: n.pitch.min(127),
                                start_beat: n.start.max(0.0) as f64,
                                length_beats: length_beats.max(0.0) as f64,
                                velocity,
                                channel,
                                expression: n.expression.sanitized(),
                                // Resolved the same way playback resolves it,
                                // so a note with no marking of its own still
                                // follows the clip's direction lane.
                                articulation: timeline_state::resolve_note_articulation(
                                    n,
                                    articulations,
                                )
                                .and_then(voicebank_articulation_for),
                                pitch_points: build_note_pitch_points(
                                    &trajectory,
                                    notes,
                                    &voice_of_note,
                                    index,
                                    length_beats,
                                    seconds_per_beat,
                                ),
                            }
                        })
                        .collect(),
                    controllers: controller_lanes
                        .iter()
                        .filter(|lane| !lane.points.is_empty())
                        .filter_map(|lane| {
                            let controller = vst3_controller_number(lane.kind)?;
                            Some(EngineMidiControllerLane {
                                controller,
                                channel: lane_channel,
                                points: lane
                                    .points
                                    .iter()
                                    .map(|p| EngineMidiControllerPoint {
                                        beat: p.beat.max(0.0) as f64,
                                        value: p.value.clamp(0.0, 1.0),
                                    })
                                    .collect(),
                            })
                        })
                        .collect(),
                    mpe: track.routing.mpe,
                })
            })
        })
        .collect();

    EngineProjectSnapshot {
        project_id: "futureboard-native".to_string(),
        project_root: project_root.map(str::to_string),
        preferred_input_device: preferred_input_device
            .map(str::to_string)
            .filter(|d| !d.trim().is_empty()),
        bpm: state.bpm.max(1.0) as f64,
        tempo_points: state
            .tempo_map
            .points
            .iter()
            .map(|p| EngineTempoPointSnapshot {
                beat: p.beat,
                bpm: p.bpm,
                curve: p.curve.to_tag(),
                tension: p.tension as f64,
            })
            .collect(),
        time_signature: [state.time_signature_num, state.time_signature_den],
        sample_rate: sample_rate.max(1),
        tracks,
        clips,
        midi_clips,
        pdc_enabled,
        latency_graph_version,
        routing: EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: sample_rate.max(1),
            buffer_size: 256,
        },
    }
}

pub(super) fn log_engine_sync_snapshot(
    snapshot: &EngineProjectSnapshot,
    dirty: bool,
    reason: &'static str,
) {
    // The MIDI forensic trace has its own flag and stays on the outside of this
    // one, so a session debugging note delivery is not forced to also print the
    // whole insert and clip inventory.
    DirectAudio::forensic_trace::log_engine_sync_midi(snapshot);
    if !crate::perf::engine_sync_debug_enabled() {
        return;
    }
    let clips_with_path = snapshot
        .clips
        .iter()
        .filter(|clip| {
            clip.media_path
                .as_deref()
                .map(|path| !path.trim().is_empty())
                .unwrap_or(false)
        })
        .count();
    let insert_count: usize = snapshot.tracks.iter().map(|t| t.inserts.len()).sum();
    let midi_note_count: usize = snapshot.midi_clips.iter().map(|c| c.notes.len()).sum();
    eprintln!(
        "[engine-sync] reason={} tracks={} clips={} clips_with_path={} inserts={} midi_clips={} midi_notes={} dirty={}",
        reason,
        snapshot.tracks.len(),
        snapshot.clips.len(),
        clips_with_path,
        insert_count,
        snapshot.midi_clips.len(),
        midi_note_count,
        dirty
    );
    for track in &snapshot.tracks {
        for insert in &track.inserts {
            eprintln!(
                "[engine-sync] insert track={} id={} kind={} enabled={} path={}",
                track.id,
                insert.id,
                insert.kind,
                insert.enabled,
                insert
                    .params
                    .get("modulePath")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<none>")
            );
        }
    }
    for clip in &snapshot.clips {
        eprintln!(
            "[engine-sync] clip id={} track={} path={} start={:.3} duration={:.3}",
            clip.id,
            clip.track_id,
            clip.media_path.as_deref().unwrap_or("<none>"),
            clip.start_beat,
            clip.duration_beats
        );
    }
}

fn track_type_name(track_type: TrackType) -> &'static str {
    match track_type {
        TrackType::Audio => "audio",
        TrackType::Midi => "midi",
        TrackType::Instrument => "instrument",
        TrackType::Bus => "bus",
        TrackType::Return => "return",
        TrackType::Group => "group",
        TrackType::Master => "master",
        TrackType::Video => "video",
    }
}

pub(super) fn volume_norm_to_linear(norm: f32) -> f32 {
    let norm = norm.clamp(0.0, 1.0);
    if norm <= 0.001 {
        return 0.0;
    }
    let db = timeline_state::volume::norm_to_db(norm);
    if db <= timeline_state::volume::MIN_DB + 0.05 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0).clamp(0.0, 2.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::edit::EditCommand;
    use crate::components::timeline::timeline_state::{CreateTrackOptions, MidiControllerKind};
    use crate::layout::plugin_bridge_runtime;

    fn instrument_state_with_clip() -> (TimelineState, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Inst".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let clip = state.build_midi_clip(&track_id, 0.0, 4.0).expect("clip");
        let clip_id = clip.id.clone();
        EditCommand::CreateClip { track_id, clip }.execute(&mut state);
        (state, clip_id)
    }

    #[test]
    fn soundfont_envelope_and_quality_reach_the_engine_snapshot() {
        use crate::components::timeline::timeline_state::{
            SoundfontEnvelope, SoundfontPlayerSettingsState, SoundfontRenderQuality,
        };

        let (mut state, _clip) = instrument_state_with_clip();
        let track_id = state.tracks[0].id.clone();
        let envelope = SoundfontEnvelope {
            attack_ms: 250.0,
            decay_ms: 500.0,
            sustain: 0.3,
            release_ms: 800.0,
        };
        assert!(state.set_track_soundfont_player_state(
            &track_id,
            SoundfontPlayerSettingsState {
                path: Some("/fonts/GM.sf2".to_string()),
                preset: Some((0, 0)),
                envelope,
                quality: SoundfontRenderQuality::High,
                ..SoundfontPlayerSettingsState::default()
            },
        ));

        let snapshot = build_engine_project_snapshot(&state, 48_000, None, None);
        let track = snapshot
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .expect("track in snapshot");
        assert!(track.builtin_soundfont_player);
        assert_eq!(track.soundfont_envelope, envelope);
        assert_eq!(track.soundfont_quality, SoundfontRenderQuality::High);
    }

    #[test]
    fn an_unchanged_soundfont_envelope_reports_no_edit() {
        use crate::components::timeline::timeline_state::SoundfontPlayerSettingsState;

        let (mut state, _clip) = instrument_state_with_clip();
        let track_id = state.tracks[0].id.clone();
        let settings = SoundfontPlayerSettingsState {
            path: Some("/fonts/GM.sf2".to_string()),
            ..SoundfontPlayerSettingsState::default()
        };
        assert!(state.set_track_soundfont_player_state(&track_id, settings.clone()));
        assert!(
            !state.set_track_soundfont_player_state(&track_id, settings),
            "republishing identical settings must not mark the project dirty"
        );
    }

    fn audio_state_with_clip() -> (TimelineState, String) {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Audio".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let clip_id = state.insert_audio_clip_with_duration(
            track_id,
            "C:/audio/loop.wav".to_string(),
            "loop".to_string(),
            0.0,
            4.0,
            Some(2.0),
        );
        (state, clip_id)
    }

    #[test]
    fn trimmed_clip_snapshot_reads_from_source_window_not_track_fader_or_legacy_offset() {
        let (mut state, clip_id) = audio_state_with_clip();
        let (_, clip) = state.find_clip(&clip_id).expect("audio clip");
        let mut stretch = clip.stretch.clone();
        stretch.original_sample_rate = 48_000;
        stretch.project_sample_rate = 48_000;
        stretch.original_duration_samples = 96_000;
        stretch.source_start_samples = 24_000;
        stretch.source_end_samples = 72_000;
        assert!(state.set_clip_stretch(&clip_id, stretch));
        assert!(state.set_clip_gain(&clip_id, 0.5));
        state.tracks[0].volume = 0.2;

        let snapshot = build_engine_project_snapshot(&state, 48_000, None, None);
        let clip = snapshot
            .clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .unwrap();
        assert!((clip.offset_seconds - 0.5).abs() < 1.0e-9);
        assert!((clip.gain - 0.5).abs() < 1.0e-6);
        assert!(
            (snapshot.tracks[0].volume - clip.gain).abs() > 0.01,
            "clip gain must remain independent from the track volume fader"
        );
    }

    #[test]
    fn overlapping_audio_clips_publish_equal_power_crossfade_durations() {
        let (mut state, first_id) = audio_state_with_clip();
        let track_id = state.tracks[0].id.clone();
        let second_id = state.insert_audio_clip_with_duration(
            track_id,
            "C:/audio/second.wav".to_string(),
            "second".to_string(),
            3.0,
            4.0,
            Some(2.0),
        );

        let snapshot = build_engine_project_snapshot(&state, 48_000, None, None);
        let first = snapshot
            .clips
            .iter()
            .find(|clip| clip.id == first_id)
            .unwrap();
        let second = snapshot
            .clips
            .iter()
            .find(|clip| clip.id == second_id)
            .unwrap();
        let first_fades = first.fades.as_ref().expect("first fade");
        let second_fades = second.fades.as_ref().expect("second fade");
        assert!((first_fades.out_duration - 0.5).abs() < 1.0e-9);
        assert!((second_fades.in_duration - 0.5).abs() < 1.0e-9);
        assert_eq!(first_fades.out_curve, "equal_power");
        assert_eq!(second_fades.in_curve, "equal_power");
    }

    #[test]
    fn armed_track_with_no_input_keeps_an_empty_engine_route() {
        use crate::audio_connections::{AudioConnectionRegistry, AvailablePorts};

        let (mut state, _) = audio_state_with_clip();
        let track = state
            .tracks
            .iter_mut()
            .find(|track| track.track_type == TrackType::Audio)
            .expect("audio track");
        track.armed = true;
        track.input_monitor = timeline_state::InputMonitorMode::Always;
        track.routing.audio_input_connection_id = None;

        let source = build_engine_input_source(
            track,
            &AudioConnectionRegistry::new(),
            &AvailablePorts::for_device("dev-1", "Interface", 4, 2),
        );
        assert!(source.device_id.is_none());
        assert!(
            source.channels.is_empty(),
            "No Input must not silently capture hardware channels 1-2"
        );
    }

    #[test]
    fn explicit_stereo_connection_preserves_device_and_ordered_channels() {
        use crate::audio_connections::{
            AudioConnectionRegistry, AvailablePorts, PhysicalInputChoice,
        };

        let (mut state, _) = audio_state_with_clip();
        let ports = AvailablePorts::for_device("asio:{driver-clsid}", "Interface", 4, 2);
        let mut registry = AudioConnectionRegistry::new();
        let id = registry
            .get_or_create_audio_connection_for_physical_input(
                &PhysicalInputChoice::Ports {
                    device_id: "asio:{driver-clsid}".to_string(),
                    channels: vec![2, 3],
                },
                &ports,
            )
            .expect("connection created");

        let track = state
            .tracks
            .iter_mut()
            .find(|track| track.track_type == TrackType::Audio)
            .expect("audio track");
        track.routing.audio_input_connection_id = Some(id);

        let source = build_engine_input_source(track, &registry, &ports);
        assert_eq!(source.device_id.as_deref(), Some("asio:{driver-clsid}"));
        assert_eq!(
            source.channels,
            vec![2, 3],
            "Left then Right order survives"
        );
    }

    /// A connection whose device vanished resolves to silence, never to
    /// another channel on a device that happens to still be present.
    #[test]
    fn a_missing_device_resolves_to_silence_not_a_fallback_channel() {
        use crate::audio_connections::{
            AudioConnectionRegistry, AvailablePorts, PhysicalInputChoice,
        };

        let (mut state, _) = audio_state_with_clip();
        let ports = AvailablePorts::for_device("unplugged", "Gone", 4, 2);
        let mut registry = AudioConnectionRegistry::new();
        let id = registry
            .get_or_create_audio_connection_for_physical_input(
                &PhysicalInputChoice::Ports {
                    device_id: "unplugged".to_string(),
                    channels: vec![0],
                },
                &ports,
            )
            .expect("connection created");
        registry.revalidate(&AvailablePorts::default());

        let track = state
            .tracks
            .iter_mut()
            .find(|track| track.track_type == TrackType::Audio)
            .expect("audio track");
        track.routing.audio_input_connection_id = Some(id);

        let source = build_engine_input_source(track, &registry, &AvailablePorts::default());
        assert!(source.device_id.is_none());
        assert!(source.channels.is_empty());
    }

    #[test]
    fn muted_notes_excluded_from_engine_snapshot() {
        let (mut state, clip_id) = instrument_state_with_clip();
        let muted = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        let _audible = state.add_midi_note(&clip_id, 64, 1.0, 1.0, 100).unwrap();
        state.set_midi_notes_muted(&clip_id, &[muted], true);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let total: usize = snap.midi_clips.iter().map(|c| c.notes.len()).sum();
        assert_eq!(total, 1, "muted note must not reach the engine snapshot");
    }

    #[test]
    fn cc_lane_reaches_engine_snapshot_with_resolved_controller() {
        let (mut state, clip_id) = instrument_state_with_clip();
        state.put_controller_point(&clip_id, MidiControllerKind::CC(11), 0.0, 0.25);
        state.put_controller_point(&clip_id, MidiControllerKind::CC(11), 2.0, 0.75);
        // Pitch bend resolves to VST3 controller 129.
        state.put_controller_point(&clip_id, MidiControllerKind::PitchBend, 1.0, 0.5);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let clip = snap
            .midi_clips
            .iter()
            .find(|c| c.id == clip_id)
            .expect("midi clip in snapshot");
        let cc11 = clip
            .controllers
            .iter()
            .find(|l| l.controller == 11)
            .expect("CC11 lane");
        assert_eq!(cc11.points.len(), 2);
        assert!(clip.controllers.iter().any(|l| l.controller == 129));
    }

    // ── MIDI articulation playback (non-destructive snapshot modifiers) ──

    fn snapshot_note(
        snap: &EngineProjectSnapshot,
        clip_id: &str,
        note_id: u64,
    ) -> EngineMidiNoteSnapshot {
        snap.midi_clips
            .iter()
            .find(|c| c.id == clip_id)
            .expect("midi clip in snapshot")
            .notes
            .iter()
            .find(|n| n.id == note_id)
            .expect("note in snapshot")
            .clone()
    }

    #[test]
    fn staccato_shortens_scheduled_gate_without_touching_note_data() {
        use crate::components::timeline::timeline_state::ArticulationId;
        let (mut state, clip_id) = instrument_state_with_clip();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 2.0, 100).unwrap();
        state.set_midi_notes_articulation(&clip_id, &[id], Some(ArticulationId::Staccato));

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let scheduled = snapshot_note(&snap, &clip_id, id);
        let gate = ArticulationId::Staccato.definition().playback.gate_ratio as f64;
        assert!((scheduled.length_beats - 2.0 * gate).abs() < 1e-6);
        assert_eq!(scheduled.velocity, 100);

        // Non-destructive: the stored note keeps its full duration/velocity.
        let note = state.midi_clip_notes(&clip_id).unwrap()[0].clone();
        assert_eq!(note.duration, 2.0);
        assert_eq!(note.velocity, 100);
    }

    #[test]
    fn accent_and_marcato_velocities_clamp_to_midi_range() {
        use crate::components::timeline::timeline_state::ArticulationId;
        let (mut state, clip_id) = instrument_state_with_clip();
        let hot = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 120).unwrap();
        let soft = state.add_midi_note(&clip_id, 64, 1.0, 1.0, 40).unwrap();
        state.set_midi_notes_articulation(&clip_id, &[hot], Some(ArticulationId::Accent));
        state.set_midi_notes_articulation(&clip_id, &[soft], Some(ArticulationId::Marcato));

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        assert_eq!(
            snapshot_note(&snap, &clip_id, hot).velocity,
            127,
            "accent on velocity 120 must clamp at 127"
        );
        let marcato_delta = ArticulationId::Marcato.definition().playback.velocity_delta as i32;
        assert_eq!(
            snapshot_note(&snap, &clip_id, soft).velocity as i32,
            40 + marcato_delta
        );
        // Stored velocities unchanged.
        let notes = state.midi_clip_notes(&clip_id).unwrap();
        assert!(notes.iter().any(|n| n.velocity == 120));
        assert!(notes.iter().any(|n| n.velocity == 40));
    }

    #[test]
    fn legato_overlaps_next_note_and_clamps_on_same_pitch() {
        use crate::components::timeline::timeline_state::{ArticulationId, LEGATO_OVERLAP_BEATS};
        let (mut state, clip_id) = instrument_state_with_clip();
        // a (60) → b (64): different pitch, overlap allowed.
        let a = state.add_midi_note(&clip_id, 60, 0.0, 0.5, 100).unwrap();
        let b = state.add_midi_note(&clip_id, 64, 1.0, 0.5, 100).unwrap();
        // c (64) → d (64): same pitch — gate must stop exactly at d's start.
        let c = state.add_midi_note(&clip_id, 64, 2.0, 0.5, 100).unwrap();
        let _d = state.add_midi_note(&clip_id, 64, 3.0, 0.5, 100).unwrap();
        state.set_midi_notes_articulation(&clip_id, &[a, b, c], Some(ArticulationId::Legato));

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let a_len = snapshot_note(&snap, &clip_id, a).length_beats;
        assert!(
            (a_len - (1.0 + LEGATO_OVERLAP_BEATS as f64)).abs() < 1e-6,
            "legato must reach the next note plus the overlap (got {a_len})"
        );
        let c_len = snapshot_note(&snap, &clip_id, c).length_beats;
        assert!(
            (c_len - 1.0).abs() < 1e-6,
            "same-pitch legato must clamp to the next note start (got {c_len})"
        );
        // b (64) is followed by c (also 64): same-pitch clamp applies to it too.
        let b_len = snapshot_note(&snap, &clip_id, b).length_beats;
        assert!((b_len - 1.0).abs() < 1e-6);
    }

    #[test]
    fn legato_without_following_note_keeps_plain_gate() {
        use crate::components::timeline::timeline_state::ArticulationId;
        let (mut state, clip_id) = instrument_state_with_clip();
        let only = state.add_midi_note(&clip_id, 60, 0.0, 1.5, 100).unwrap();
        state.set_midi_notes_articulation(&clip_id, &[only], Some(ArticulationId::Legato));

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let len = snapshot_note(&snap, &clip_id, only).length_beats;
        assert!(
            (len - 1.5).abs() < 1e-6,
            "last legato note must not grow an unbounded tail (got {len})"
        );
    }

    #[test]
    fn direction_articulation_is_chased_per_note_start() {
        use crate::components::timeline::timeline_state::ArticulationId;
        let (mut state, clip_id) = instrument_state_with_clip();
        let before = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        let after = state.add_midi_note(&clip_id, 62, 2.0, 1.0, 100).unwrap();
        let later = state.add_midi_note(&clip_id, 64, 3.0, 1.0, 100).unwrap();
        // Staccato direction starting at beat 2; nothing before it.
        state.add_midi_articulation(&clip_id, 2.0, ArticulationId::Staccato);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let gate = ArticulationId::Staccato.definition().playback.gate_ratio as f64;
        assert!(
            (snapshot_note(&snap, &clip_id, before).length_beats - 1.0).abs() < 1e-6,
            "note before the direction event must be unaffected"
        );
        // Both notes at/after the event chase the same direction — including
        // `later`, which starts after the event with no event of its own
        // (equivalent to starting playback mid-clip).
        assert!((snapshot_note(&snap, &clip_id, after).length_beats - gate).abs() < 1e-6);
        assert!((snapshot_note(&snap, &clip_id, later).length_beats - gate).abs() < 1e-6);
    }

    #[test]
    fn per_note_articulation_overrides_direction_lane() {
        use crate::components::timeline::timeline_state::ArticulationId;
        let (mut state, clip_id) = instrument_state_with_clip();
        let id = state.add_midi_note(&clip_id, 60, 1.0, 1.0, 100).unwrap();
        state.add_midi_articulation(&clip_id, 0.0, ArticulationId::Staccato);
        state.set_midi_notes_articulation(&clip_id, &[id], Some(ArticulationId::Tenuto));

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let len = snapshot_note(&snap, &clip_id, id).length_beats;
        assert!(
            (len - 1.0).abs() < 1e-6,
            "per-note tenuto (gate 1.0) must override the staccato direction"
        );
    }

    #[test]
    fn offline_export_snapshot_applies_identical_articulation() {
        use crate::components::timeline::timeline_state::ArticulationId;
        let (mut state, clip_id) = instrument_state_with_clip();
        let a = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        let b = state.add_midi_note(&clip_id, 64, 1.0, 1.0, 100).unwrap();
        state.set_midi_notes_articulation(&clip_id, &[a], Some(ArticulationId::Legato));
        state.add_midi_articulation(&clip_id, 0.0, ArticulationId::Marcato);

        let live = build_engine_project_snapshot(&state, 48_000, None, None);
        let export = build_engine_project_snapshot_for_export(&state, 48_000, None, None, true, 0);
        for id in [a, b] {
            let l = snapshot_note(&live, &clip_id, id);
            let e = snapshot_note(&export, &clip_id, id);
            assert_eq!(l.length_beats, e.length_beats);
            assert_eq!(l.velocity, e.velocity);
            assert_eq!(l.start_beat, e.start_beat);
        }
    }

    #[test]
    fn audio_unit_insert_reaches_the_engine_as_an_au_bridge_descriptor() {
        use crate::components::timeline::timeline_state::InsertPluginFormat;

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "FX".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        let component = "au:61756678:64656c79:6170706c";
        // An Audio Unit stores its component id where a VST3 stores a module
        // path; nothing on disk answers to that string.
        state.set_insert_plugin(
            &track_id,
            &slot,
            component.to_string(),
            Some(std::path::PathBuf::from(component)),
            InsertPluginFormat::Au,
            None,
            "AUDelay".to_string(),
        );

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let insert = snap
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.inserts.first())
            .expect("audio unit insert must survive the graph build");
        assert_eq!(insert.kind, "external-bridge-plugin");
        assert_eq!(
            insert
                .params
                .get("format")
                .and_then(serde_json::Value::as_str),
            Some("AU"),
            "the engine must see the real format, not VST3"
        );
        assert_eq!(
            insert
                .params
                .get("classId")
                .and_then(serde_json::Value::as_str),
            Some(component)
        );
        assert_eq!(
            insert
                .params
                .get("modulePath")
                .and_then(serde_json::Value::as_str),
            Some(""),
            "an Audio Unit has no module path to hand the host"
        );
    }

    #[test]
    fn graph_snapshot_retains_all_vst_inserts_in_order() {
        use crate::components::timeline::timeline_state::InsertPluginFormat;

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "FX".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let slot_a = state.ensure_insert_slot_at(&track_id, 0).expect("slot A");
        let slot_b = state.ensure_insert_slot_at(&track_id, 1).expect("slot B");
        state.set_insert_plugin(
            &track_id,
            &slot_a,
            "class-a".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/a.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Plugin A".to_string(),
        );
        state.set_insert_plugin(
            &track_id,
            &slot_b,
            "class-b".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/b.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Plugin B".to_string(),
        );

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let track = snap
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .expect("audio track in snapshot");
        assert_eq!(
            track.inserts.len(),
            2,
            "both inserts must survive graph build"
        );
        assert_eq!(track.inserts[0].id, slot_a);
        assert_eq!(track.inserts[1].id, slot_b);
    }

    #[test]
    fn vsti_multiout_children_are_stable_tracks_and_engine_routes() {
        use crate::components::timeline::timeline_state::{
            vsti_output_child_track_id, InsertPluginFormat,
        };

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Drums".to_string(),
            color: gpui::Rgba {
                r: 0.2,
                g: 0.3,
                b: 0.4,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "multiout-class".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/MultiOut.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "MultiOut".to_string(),
        );

        assert!(state.set_insert_output_bus_layout(&track_id, &slot, &[2, 2, 2, 2]));
        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 8));
        let bus_0_id = vsti_output_child_track_id(&slot, 0);
        let bus_1_id = vsti_output_child_track_id(&slot, 1);
        let bus_3_id = vsti_output_child_track_id(&slot, 3);
        assert!(state.tracks.iter().any(|track| track.id == bus_0_id));
        assert!(state.tracks.iter().any(|track| track.id == bus_1_id));
        assert!(state.tracks.iter().any(|track| track.id == bus_3_id));

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let parent = snap
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("parent track");
        let insert = parent
            .inserts
            .iter()
            .find(|insert| insert.id == slot)
            .expect("parent insert");
        let children = insert
            .params
            .get("vstiOutputChildren")
            .and_then(|value| value.as_array())
            .expect("vsti children");
        assert_eq!(children.len(), 4);
        assert!(children.iter().any(|child| {
            child.get("busIndex").and_then(|v| v.as_u64()) == Some(0)
                && child.get("trackId").and_then(|v| v.as_str()) == Some(bus_0_id.as_str())
                && child.get("channelCount").and_then(|v| v.as_u64()) == Some(2)
                && child.get("channelL").and_then(|v| v.as_u64()) == Some(1)
                && child.get("channelR").and_then(|v| v.as_u64()) == Some(2)
        }));
        assert!(children.iter().any(|child| {
            child.get("busIndex").and_then(|v| v.as_u64()) == Some(3)
                && child.get("trackId").and_then(|v| v.as_str()) == Some(bus_3_id.as_str())
                && child.get("mixerChannelId").and_then(|v| v.as_str()) == Some(bus_3_id.as_str())
                && child.get("routeNodeId").and_then(|v| v.as_str()) == Some(bus_3_id.as_str())
                && child.get("channelCount").and_then(|v| v.as_u64()) == Some(2)
                && child.get("channelL").and_then(|v| v.as_u64()) == Some(7)
                && child.get("channelR").and_then(|v| v.as_u64()) == Some(8)
        }));
    }

    /// An FX insert on a VSTi multi-out child strip must reach the engine
    /// graph: the child is a real Bus track in the snapshot, its insert chain
    /// serializes like any other track's, and `enabled` mirrors bypass. This
    /// is the processing contract behind the mixer sub-strip insert rack.
    #[test]
    fn substrip_fx_insert_serializes_into_engine_snapshot() {
        use crate::components::timeline::timeline_state::{
            vsti_output_child_track_id, InsertPluginFormat,
        };

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Drums".to_string(),
            color: gpui::Rgba {
                r: 0.2,
                g: 0.3,
                b: 0.4,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "multiout-class".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/MultiOut.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "MultiOut".to_string(),
        );
        assert!(state.set_insert_output_bus_layout(&track_id, &slot, &[2, 2]));
        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 4));

        let child_id = vsti_output_child_track_id(&slot, 1);
        let fx_slot = state.add_insert(&child_id).expect("substrip insert slot");
        state.set_insert_plugin(
            &child_id,
            &fx_slot,
            "comp-class".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/Comp.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Comp".to_string(),
        );
        state.toggle_insert_bypass(&child_id, &fx_slot);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let child = snap
            .tracks
            .iter()
            .find(|track| track.id == child_id)
            .expect("child bus track in snapshot");
        let fx = child
            .inserts
            .iter()
            .find(|insert| insert.id == fx_slot)
            .expect("substrip insert in snapshot");
        assert!(
            !fx.enabled,
            "bypassed substrip insert must serialize enabled=false"
        );
        // The other child strip carries no insert chain.
        let sibling_id = vsti_output_child_track_id(&slot, 0);
        let sibling = snap
            .tracks
            .iter()
            .find(|track| track.id == sibling_id)
            .expect("sibling child track");
        assert!(sibling.inserts.is_empty());
    }

    #[test]
    fn single_multichannel_vsti_bus_exports_flat_pair_children() {
        use crate::components::timeline::timeline_state::{
            vsti_output_child_track_id, InsertPluginFormat,
        };

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "MT Power".to_string(),
            color: gpui::Rgba {
                r: 0.2,
                g: 0.3,
                b: 0.4,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "single-bus-multiout-class".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/MTPower.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "MT Power".to_string(),
        );

        assert!(state.set_insert_output_bus_layout(&track_id, &slot, &[8]));
        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 8));

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let parent = snap
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("parent track");
        let insert = parent
            .inserts
            .iter()
            .find(|insert| insert.id == slot)
            .expect("parent insert");
        let children = insert
            .params
            .get("vstiOutputChildren")
            .and_then(|value| value.as_array())
            .expect("vsti children");
        assert_eq!(children.len(), 4);

        let bus_1_id = vsti_output_child_track_id(&slot, 1);
        assert!(children.iter().any(|child| {
            child.get("busIndex").and_then(|v| v.as_u64()) == Some(1)
                && child.get("trackId").and_then(|v| v.as_str()) == Some(bus_1_id.as_str())
                && child.get("channelCount").and_then(|v| v.as_u64()) == Some(2)
                && child.get("channelL").and_then(|v| v.as_u64()) == Some(3)
                && child.get("channelR").and_then(|v| v.as_u64()) == Some(4)
        }));
    }

    #[test]
    fn resample_snapshot_ignores_preserve_pitch() {
        let (mut state, clip_id) = audio_state_with_clip();
        let mut stretch = state.clip_stretch(&clip_id).cloned().unwrap();
        stretch.mode = StretchMode::Resample;
        stretch.preserve_pitch = true;
        state.set_clip_stretch(&clip_id, stretch);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let process = snap.clips[0].audio_process.as_ref().unwrap();
        assert_eq!(process.mode, "resample");
        assert!(!process.preserve_pitch);
    }

    #[test]
    fn manual_snapshot_routes_preserve_pitch_and_pitch_values() {
        let (mut state, clip_id) = audio_state_with_clip();
        let mut stretch = state.clip_stretch(&clip_id).cloned().unwrap();
        stretch.mode = StretchMode::Manual;
        stretch.preserve_pitch = true;
        stretch.pitch_shift_semitones = 12.5;
        state.set_clip_stretch(&clip_id, stretch);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let process = snap.clips[0].audio_process.as_ref().unwrap();
        assert_eq!(process.mode, "manual");
        assert!(process.preserve_pitch);
        assert!((process.pitch_semitones - 12.5).abs() < 1e-6);
        assert!(
            (process.pitch_ratio
                - timeline_state::AudioClipStretchState::pitch_ratio_from_semitones(12.5))
            .abs()
                < 1e-6
        );
    }

    #[test]
    fn warp_markers_reach_engine_snapshot_sorted() {
        let (mut state, clip_id) = audio_state_with_clip();
        let mut stretch = state.clip_stretch(&clip_id).cloned().unwrap();
        stretch.mode = StretchMode::Warp;
        stretch.set_stretch_ratio(2.0);
        stretch.warp_markers = vec![
            timeline_state::WarpMarker {
                id: 2,
                source_sample: 2_000,
                timeline_beat: 3.0,
                locked: false,
            },
            timeline_state::WarpMarker {
                id: 1,
                source_sample: 1_000,
                timeline_beat: 1.0,
                locked: true,
            },
        ];
        state.set_clip_stretch(&clip_id, stretch);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let process = snap.clips[0].audio_process.as_ref().unwrap();
        assert_eq!(process.mode, "warp");
        assert_eq!(process.warp_markers.len(), 2);
        assert_eq!(process.warp_markers[0].id, 1);
        assert_eq!(process.warp_markers[1].id, 2);
        assert!((process.effective_time_ratio - 2.0).abs() < 1e-9);
    }

    #[test]
    fn export_snapshot_uses_bridge_inserts_when_bridge_enabled() {
        use crate::components::timeline::timeline_state::InsertPluginFormat;

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "FX".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let slot = state.ensure_insert_slot_at(&track_id, 0).expect("slot");
        state.set_insert_plugin(
            &track_id,
            &slot,
            "class-a".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/a.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Plugin A".to_string(),
        );
        let state_bytes = vec![9u8, 8, 7, 6];
        for track in &mut state.tracks {
            for ins in &mut track.inserts {
                if ins.id == slot {
                    ins.vst3_state = Some(std::sync::Arc::new(state_bytes.clone()));
                }
            }
        }

        if plugin_bridge_runtime::bridge_enabled() {
            let exported =
                build_engine_project_snapshot_for_export(&state, 48_000, None, None, true, 0);
            let insert = exported
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .and_then(|t| t.inserts.iter().find(|i| i.id == slot))
                .expect("insert in export snapshot");
            assert_eq!(
                insert.kind, "external-bridge-plugin",
                "export must mirror live bridge inserts so offline render can drive bridge sinks"
            );
            assert!(
                insert.state.is_none(),
                "bridge-owned inserts do not carry in-process restore blobs"
            );
        } else {
            let exported =
                build_engine_project_snapshot_for_export(&state, 48_000, None, None, true, 0);
            let insert = exported
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .and_then(|t| t.inserts.iter().find(|i| i.id == slot))
                .expect("insert in export snapshot");
            assert_eq!(insert.kind, "native-plugin");
            assert_eq!(insert.state.as_deref(), Some(state_bytes.as_slice()));
        }

        // Live snapshot never carries the export state (bridged host owns restore).
        let live = build_engine_project_snapshot(&state, 48_000, None, None);
        if let Some(live_insert) = live
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.inserts.iter().find(|i| i.id == slot))
        {
            assert!(
                live_insert.state.is_none(),
                "live snapshot must not carry export state"
            );
        }
    }

    #[test]
    fn instrument_track_marks_only_first_bridge_insert_as_instrument() {
        use crate::components::timeline::timeline_state::InsertPluginFormat;

        let mut state = TimelineState::default();
        state.tracks.clear();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Instrument".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let slot_instrument = state
            .ensure_insert_slot_at(&track_id, 0)
            .expect("vsti slot");
        let slot_effect = state.ensure_insert_slot_at(&track_id, 1).expect("fx slot");
        state.set_insert_plugin(
            &track_id,
            &slot_instrument,
            "synth-class".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/synth.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "Synth".to_string(),
        );
        state.set_insert_plugin(
            &track_id,
            &slot_effect,
            "fx-class".to_string(),
            Some(std::path::PathBuf::from("C:/plugins/fx.vst3")),
            InsertPluginFormat::Vst3,
            None,
            "FX".to_string(),
        );

        let track = state
            .find_track(&track_id)
            .expect("instrument track in state");
        assert_eq!(
            bridge_insert_role(track.track_type, 0, None, false),
            "instrument"
        );
        assert_eq!(
            bridge_insert_role(track.track_type, 1, None, false),
            "effect"
        );

        state.set_insert_plugin_role(&track_id, &slot_instrument, false);
        let snapshot = build_engine_project_snapshot(&state, 48_000, None, None);
        let track = snapshot
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("instrument track in engine snapshot");
        assert_eq!(
            track.inserts[0]
                .params
                .get("role")
                .and_then(serde_json::Value::as_str),
            Some("effect"),
            "registry role must override the legacy slot-zero heuristic"
        );
    }

    /// A plug-in that declares itself an instrument still runs as an effect on a
    /// track that carries audio rather than notes.
    ///
    /// The role picks the bridge's processing shape, not a label: `"instrument"`
    /// makes the engine skip `write_input` and *add* the plugin's output to the
    /// dry block instead of replacing it, so a mis-declaring VST3 (an EQ whose
    /// `subCategories` lead with `Instrument`) inserted on an audio track was fed
    /// silence and left the dry signal untouched — an insert that did nothing.
    #[test]
    fn instrument_declaration_is_ignored_on_tracks_that_carry_audio() {
        for track_type in [
            TrackType::Audio,
            TrackType::Bus,
            TrackType::Return,
            TrackType::Group,
            TrackType::Master,
        ] {
            assert_eq!(
                bridge_insert_role(track_type, 0, Some(true), false),
                "effect",
                "{track_type:?} has no note source, so slot 0 cannot be an instrument"
            );
            assert_eq!(
                bridge_insert_role(track_type, 2, Some(true), false),
                "effect"
            );
            assert_eq!(bridge_insert_role(track_type, 0, None, false), "effect");
        }
        // Tracks that do carry notes keep trusting the declaration.
        for track_type in [TrackType::Instrument, TrackType::Midi] {
            assert_eq!(
                bridge_insert_role(track_type, 0, Some(true), false),
                "instrument"
            );
            assert_eq!(
                bridge_insert_role(track_type, 3, Some(true), false),
                "instrument"
            );
            assert_eq!(
                bridge_insert_role(track_type, 0, Some(false), false),
                "effect"
            );
            assert_eq!(bridge_insert_role(track_type, 0, None, false), "instrument");
            assert_eq!(bridge_insert_role(track_type, 1, None, false), "effect");
        }
    }

    /// On a Soundfont Player / Solfege track the note source is the track
    /// itself, so every insert — slot 0 included, declared or not — is an
    /// effect that must be fed the instrument's audio.
    #[test]
    fn inserts_on_a_builtin_instrument_track_are_always_effects() {
        for track_type in [TrackType::Instrument, TrackType::Midi] {
            assert_eq!(bridge_insert_role(track_type, 0, None, true), "effect");
            assert_eq!(
                bridge_insert_role(track_type, 0, Some(true), true),
                "effect"
            );
            assert_eq!(
                bridge_insert_role(track_type, 1, Some(false), true),
                "effect"
            );
        }
    }

    #[test]
    fn empty_and_poly_pressure_lanes_are_omitted() {
        let (mut state, clip_id) = instrument_state_with_clip();
        // Ensure an empty lane and a poly-pressure lane (no global mapping).
        state.ensure_controller_lane(&clip_id, MidiControllerKind::CC(7));
        state.put_controller_point(&clip_id, MidiControllerKind::PolyPressure, 0.0, 0.5);

        let snap = build_engine_project_snapshot(&state, 48_000, None, None);
        let clip = snap.midi_clips.iter().find(|c| c.id == clip_id).unwrap();
        assert!(
            clip.controllers.is_empty(),
            "empty CC7 lane and unmapped poly-pressure lane must be omitted"
        );
    }

    fn snapshot_signature(state: &TimelineState, input_device: Option<&str>) -> String {
        let snapshot = build_engine_project_snapshot(state, 48_000, None, input_device);
        serde_json::to_string(&snapshot).unwrap()
    }

    /// R4: `None` and `""` (and whitespace) for the input device must normalize to
    /// the same graph, so re-opening AudioSettings with an unchanged/empty device
    /// never produces a different signature → never forces an engine resync.
    #[test]
    fn input_device_none_and_empty_produce_identical_graph() {
        let (state, _clip) = audio_state_with_clip();
        let sig_none = snapshot_signature(&state, None);
        let sig_empty = snapshot_signature(&state, Some(""));
        let sig_ws = snapshot_signature(&state, Some("   "));
        assert_eq!(sig_none, sig_empty, "None and \"\" must normalize equal");
        assert_eq!(sig_none, sig_ws, "None and whitespace must normalize equal");

        use crate::layout::audio_transport::graph_fingerprint_of;
        assert_eq!(
            graph_fingerprint_of(&sig_none),
            graph_fingerprint_of(&sig_empty),
            "equal graphs must share a fingerprint → deduped, no second rebuild"
        );
    }

    /// R9 (unit-level): the graph fingerprint is deterministic and equal for an
    /// unchanged graph, which is what lets `schedule_audio_project_sync` skip a
    /// duplicate route-graph rebuild / `load_project` for the same graph. A real
    /// change (a new track) must change the fingerprint so the rebuild still runs.
    #[test]
    fn graph_fingerprint_is_stable_and_change_sensitive() {
        use crate::layout::audio_transport::graph_fingerprint_of;
        let (mut state, _clip) = audio_state_with_clip();

        let fp1 = graph_fingerprint_of(&snapshot_signature(&state, None));
        let fp2 = graph_fingerprint_of(&snapshot_signature(&state, None));
        assert_eq!(fp1, fp2, "identical graph must fingerprint identically");

        state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Added".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: timeline_state::InputMonitorMode::Off,
        });
        let fp3 = graph_fingerprint_of(&snapshot_signature(&state, None));
        assert_ne!(fp1, fp3, "a real graph change must change the fingerprint");
    }
}
