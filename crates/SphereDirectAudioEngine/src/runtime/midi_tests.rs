use super::*;
use crate::types::{
    EngineAutomationLaneSnapshot, EngineMidiClipSnapshot, EngineMidiControllerLane,
    EngineMidiControllerPoint, EngineMidiNoteSnapshot,
};

fn clip_with_one_note() -> EngineMidiClipSnapshot {
    EngineMidiClipSnapshot {
        source_track_id: None,
        id: "mc1".into(),
        track_id: "track-1".into(),
        start_beat: 4.0, // bar 2 in 4/4
        length_beats: 4.0,
        notes: vec![EngineMidiNoteSnapshot {
            id: 1,
            pitch: 60, // C4
            start_beat: 0.0,
            length_beats: 1.0,
            velocity: 100,
            channel: 0,
            expression: sphere_midi_service::NoteExpression::default(),
            pitch_points: Vec::new(),
            articulation: None,
        }],
        controllers: Vec::new(),
        mpe: sphere_midi_service::mpe::MpeTrackConfiguration::default(),
    }
}

fn project_with(clips: Vec<EngineMidiClipSnapshot>) -> RuntimeProject {
    let tempo_map = RuntimeTempoMapSnapshot::static_tempo(120.0);
    let (midi_clips, midi_tracks) =
        build_midi_runtime(&shared_midi_clips(&clips), &tempo_map, 48_000);
    RuntimeProject {
        spatial: Default::default(),
        sample_rate: 48_000,
        tempo_map,
        midi_clips,
        midi_tracks,
        ..Default::default()
    }
}

#[test]
fn note_resolves_to_absolute_samples_with_off_before_on() {
    let p = project_with(vec![clip_with_one_note()]);
    let evs = &p.midi_tracks[0].events;
    assert_eq!(evs.len(), 2);
    // absolute start beat = 4 + 0 = 4 → 96000 sa; end beat 5 → 120000 sa.
    let on = evs
        .iter()
        .find(|e| e.kind == RuntimeMidiEventKind::NoteOn)
        .unwrap();
    let off = evs
        .iter()
        .find(|e| e.kind == RuntimeMidiEventKind::NoteOff)
        .unwrap();
    assert_eq!(on.sample, 96_000);
    assert_eq!(off.sample, 120_000);
    assert_eq!(on.pitch, 60);
    assert_eq!(on.velocity, 100);
}

#[test]
fn note_expression_is_scheduled_on_an_allocated_mpe_member_channel() {
    let mut clip = clip_with_one_note();
    clip.notes[0].expression = sphere_midi_service::NoteExpression {
        pitch: sphere_midi_service::ExpressionCurve::from_points(vec![
            sphere_midi_service::ExpressionPoint::new(0.0, 0.0),
            sphere_midi_service::ExpressionPoint::new(0.5, 0.5),
        ]),
        pressure: sphere_midi_service::ExpressionCurve::from_points(vec![
            sphere_midi_service::ExpressionPoint::new(0.0, 0.75),
        ]),
        timbre: sphere_midi_service::ExpressionCurve::from_points(vec![
            sphere_midi_service::ExpressionPoint::new(0.0, 0.25),
        ]),
        ..Default::default()
    };
    let p = project_with(vec![clip]);
    let evs = &p.midi_tracks[0].events;
    assert!(evs.iter().any(|event| {
        event.kind == RuntimeMidiEventKind::ControlChange
            && event.cc_number == 129
            && event.channel == 1
            && (event.cc_value - 0.5).abs() < 0.001
    }));
    assert!(evs.iter().any(|event| {
        event.kind == RuntimeMidiEventKind::ControlChange
            && event.cc_number == 128
            && event.channel == 1
            && (event.cc_value - 0.75).abs() < 0.001
    }));
    let note_on = evs
        .iter()
        .find(|event| event.kind == RuntimeMidiEventKind::NoteOn)
        .unwrap();
    assert_eq!(note_on.channel, 1);
}

#[test]
fn release_velocity_reaches_the_runtime_note_off_event() {
    let mut clip = clip_with_one_note();
    clip.notes[0].expression.release_velocity = Some(0.5);
    let p = project_with(vec![clip]);
    let note_off = p.midi_tracks[0]
        .events
        .iter()
        .find(|event| event.kind == RuntimeMidiEventKind::NoteOff)
        .unwrap();
    assert_eq!(note_off.velocity, 64);
}

#[test]
fn explicit_upper_zone_routes_notes_and_emits_zone_configuration() {
    let mut clip = clip_with_one_note();
    clip.mpe = sphere_midi_service::mpe::MpeTrackConfiguration {
        mode: sphere_midi_service::mpe::MpeOutputMode::Upper,
        member_channels: 2,
        member_pitch_range: 12.0,
        manager_pitch_range: 24.0,
    };
    let p = project_with(vec![clip]);
    let evs = &p.midi_tracks[0].events;
    let note_on = evs
        .iter()
        .find(|event| event.kind == RuntimeMidiEventKind::NoteOn)
        .unwrap();
    assert_eq!(note_on.channel, 13);
    assert!(evs.iter().any(|event| {
        event.kind == RuntimeMidiEventKind::ControlChange
            && event.channel == 15
            && event.cc_number == 100
            && (event.cc_value - 6.0 / 127.0).abs() < 0.001
    }));
    assert!(evs.iter().any(|event| {
        event.kind == RuntimeMidiEventKind::ControlChange
            && event.channel == 15
            && event.cc_number == 6
            && (event.cc_value - 2.0 / 127.0).abs() < 0.001
    }));
    assert!(evs.iter().any(|event| {
        event.kind == RuntimeMidiEventKind::ControlChange
            && event.channel == 13
            && event.cc_number == 6
            && (event.cc_value - 12.0 / 127.0).abs() < 0.001
    }));
}

#[test]
fn overlapping_mpe_clips_share_member_channels_per_output_track() {
    let mut first = clip_with_one_note();
    first.mpe = sphere_midi_service::mpe::MpeTrackConfiguration {
        mode: sphere_midi_service::mpe::MpeOutputMode::Lower,
        member_channels: 2,
        member_pitch_range: 2.0,
        manager_pitch_range: 2.0,
    };
    first.notes[0].length_beats = 2.0;

    let mut second = clip_with_one_note();
    second.id = "mc2".into();
    second.start_beat = 4.5;
    second.mpe = first.mpe;

    // Reverse the input order to prove allocation follows musical order,
    // not serialization order.
    let p = project_with(vec![second, first]);
    let note_ons = p.midi_tracks[0]
        .events
        .iter()
        .filter(|event| event.kind == RuntimeMidiEventKind::NoteOn)
        .collect::<Vec<_>>();
    assert_eq!(note_ons.len(), 2);
    assert_eq!(note_ons[0].channel, 1);
    assert_eq!(note_ons[1].channel, 2);
}

#[test]
fn stolen_mpe_note_cuts_events_from_an_earlier_clip() {
    let mut first = clip_with_one_note();
    first.mpe = sphere_midi_service::mpe::MpeTrackConfiguration {
        mode: sphere_midi_service::mpe::MpeOutputMode::Lower,
        member_channels: 1,
        member_pitch_range: 2.0,
        manager_pitch_range: 2.0,
    };
    first.notes[0].length_beats = 3.0;
    first.notes[0].expression.pitch = sphere_midi_service::ExpressionCurve::from_points(vec![
        sphere_midi_service::ExpressionPoint::new(1.5, 0.75),
    ]);

    let mut second = clip_with_one_note();
    second.id = "mc2".into();
    second.start_beat = 4.5;
    second.mpe = first.mpe;

    let p = project_with(vec![first, second]);
    let steal_sample = 108_000; // beat 4.5 at 120 BPM
    assert!(p.midi_tracks[0]
        .events
        .iter()
        .all(|event| { event.note_id != 1 || event.sample <= steal_sample }));
    assert!(p.midi_tracks[0].events.iter().any(|event| {
        event.note_id == 1
            && event.kind == RuntimeMidiEventKind::NoteOff
            && event.sample == steal_sample
    }));
}

#[test]
fn same_time_mpe_voice_steal_does_not_emit_the_displaced_note_on() {
    let mut first = clip_with_one_note();
    first.mpe = sphere_midi_service::mpe::MpeTrackConfiguration {
        mode: sphere_midi_service::mpe::MpeOutputMode::Lower,
        member_channels: 1,
        member_pitch_range: 2.0,
        manager_pitch_range: 2.0,
    };
    first.notes[0].pitch = 60;
    first.notes[0].length_beats = 1.0;

    let mut second = clip_with_one_note();
    second.id = "mc2".into();
    second.notes[0].pitch = 64;
    second.mpe = first.mpe;

    let p = project_with(vec![first, second]);
    let note_ons = p.midi_tracks[0]
        .events
        .iter()
        .filter(|event| event.kind == RuntimeMidiEventKind::NoteOn)
        .collect::<Vec<_>>();
    assert_eq!(note_ons.len(), 1);
    assert_eq!(note_ons[0].note_id, 2);
    assert_eq!(note_ons[0].pitch, 64);
}

#[test]
fn reused_mpe_channel_gets_expression_reset_before_the_next_note() {
    let mut clip = clip_with_one_note();
    clip.notes.push(EngineMidiNoteSnapshot {
        id: 2,
        pitch: 64,
        start_beat: 2.0,
        length_beats: 1.0,
        velocity: 100,
        channel: 0,
        expression: Default::default(),
        pitch_points: Vec::new(),
        articulation: None,
    });
    clip.notes[0].expression.pitch = sphere_midi_service::ExpressionCurve::from_points(vec![
        sphere_midi_service::ExpressionPoint::new(0.0, 0.8),
    ]);
    let p = project_with(vec![clip]);
    let evs = &p.midi_tracks[0].events;
    let second_start = 144_000; // clip beat 2 at 120 BPM
    let reset = evs
        .iter()
        .filter(|event| {
            event.sample == second_start
                && event.channel == 1
                && event.kind == RuntimeMidiEventKind::ControlChange
        })
        .collect::<Vec<_>>();
    assert!(reset
        .iter()
        .any(|event| { event.cc_number == 129 && (event.cc_value - 0.5).abs() < 0.001 }));
    assert!(reset
        .iter()
        .any(|event| event.cc_number == 128 && event.cc_value == 0.0));
    assert!(reset
        .iter()
        .any(|event| event.cc_number == 74 && event.cc_value == 0.0));
}

#[test]
fn mpe_channel_stays_reserved_until_sustain_pedal_up() {
    let mut clip = clip_with_one_note();
    clip.notes[0].expression.pitch = sphere_midi_service::ExpressionCurve::from_points(vec![
        sphere_midi_service::ExpressionPoint::new(0.0, 0.25),
    ]);
    clip.notes.push(EngineMidiNoteSnapshot {
        id: 2,
        pitch: 64,
        start_beat: 1.5,
        length_beats: 1.0,
        velocity: 100,
        channel: 0,
        expression: Default::default(),
        pitch_points: Vec::new(),
        articulation: None,
    });
    clip.controllers.push(EngineMidiControllerLane {
        controller: 64,
        channel: 0,
        points: vec![
            EngineMidiControllerPoint {
                beat: 0.5,
                value: 1.0,
            },
            EngineMidiControllerPoint {
                beat: 2.0,
                value: 0.0,
            },
        ],
    });
    let p = project_with(vec![clip]);
    let note_ons = p.midi_tracks[0]
        .events
        .iter()
        .filter(|event| event.kind == RuntimeMidiEventKind::NoteOn)
        .collect::<Vec<_>>();
    assert_eq!(note_ons.len(), 2);
    assert_eq!(note_ons[0].channel, 1);
    assert_eq!(note_ons[1].channel, 2);
}

#[test]
fn sustain_state_is_shared_across_overlapping_mpe_clips() {
    let mut first = clip_with_one_note();
    first.mpe = sphere_midi_service::mpe::MpeTrackConfiguration {
        mode: sphere_midi_service::mpe::MpeOutputMode::Lower,
        member_channels: 1,
        member_pitch_range: 2.0,
        manager_pitch_range: 2.0,
    };
    first.start_beat = 0.0;
    first.length_beats = 1.0;
    first.notes[0].length_beats = 1.0;
    first.controllers = vec![EngineMidiControllerLane {
        controller: 64,
        channel: 0,
        points: vec![EngineMidiControllerPoint {
            beat: 0.0,
            value: 1.0,
        }],
    }];

    let mut second = clip_with_one_note();
    second.id = "mc2".into();
    second.start_beat = 1.5;
    second.length_beats = 1.0;
    second.mpe = first.mpe;

    let p = project_with(vec![first, second]);
    let steal_sample = 36_000; // beat 1.5 at 120 BPM
    assert!(p.midi_tracks[0].events.iter().any(|event| {
        event.note_id == 1
            && event.kind == RuntimeMidiEventKind::NoteOff
            && event.sample == steal_sample
    }));
}

#[test]
fn zero_length_note_is_skipped() {
    let mut clip = clip_with_one_note();
    clip.notes[0].length_beats = 0.0;
    let p = project_with(vec![clip]);
    assert!(p.midi_tracks.is_empty() || p.midi_tracks[0].events.is_empty());
}

#[test]
fn schedule_fires_note_on_then_off_and_tracks_active() {
    let mut p = project_with(vec![clip_with_one_note()]);
    p.reset_midi_playback(0);
    // Block before the note: nothing active.
    p.schedule_midi_block(0, 512);
    assert_eq!(p.midi_tracks[0].active.len(), 0);
    // Block covering the NoteOn (96000).
    p.schedule_midi_block(96_000, 512);
    assert_eq!(p.midi_tracks[0].active, vec![(0u8, 60u8)]);
    // Block covering the NoteOff (120000).
    p.schedule_midi_block(120_000, 512);
    assert!(p.midi_tracks[0].active.is_empty());
}

#[test]
fn seek_before_note_then_play_fires_it() {
    let mut p = project_with(vec![clip_with_one_note()]);
    p.reset_midi_playback(95_000); // just before the NoteOn
    p.schedule_midi_block(95_000, 2048); // covers 95000..97048 → fires NoteOn
    assert_eq!(p.midi_tracks[0].active, vec![(0u8, 60u8)]);
}

#[test]
fn seek_after_note_does_not_fire_old_note() {
    let mut p = project_with(vec![clip_with_one_note()]);
    p.reset_midi_playback(200_000); // well past the note
    p.schedule_midi_block(200_000, 512);
    assert!(p.midi_tracks[0].active.is_empty());
    assert_eq!(p.midi_tracks[0].cursor, p.midi_tracks[0].events.len());
}

#[test]
fn all_notes_off_clears_active() {
    let mut p = project_with(vec![clip_with_one_note()]);
    p.reset_midi_playback(96_000);
    p.schedule_midi_block(96_000, 512);
    assert_eq!(p.midi_tracks[0].active.len(), 1);
    p.all_notes_off("stop");
    assert!(p.midi_tracks[0].active.is_empty());
}

#[test]
fn mute_and_solo_leave_sounding_notes_alone() {
    // Mute/solo silence a track's output, not its instrument: neither may
    // release the notes that are already sounding, and scheduling keeps
    // running underneath so the parts stay in sync with the transport.
    let mut clip2 = clip_with_one_note();
    clip2.id = "mc2".into();
    clip2.track_id = "track-2".into();
    let mut p = project_with(vec![clip_with_one_note(), clip2]);
    p.tracks = vec![
        bridged_instrument_track("track-1"),
        bridged_instrument_track("track-2"),
    ];
    p.resolve_indices();
    p.reset_midi_playback(96_000);
    p.schedule_midi_block(96_000, 512);
    assert_eq!(p.midi_tracks[0].active.len(), 1);
    assert_eq!(p.midi_tracks[1].active.len(), 1);

    p.update_track_mute("track-1", true);
    assert_eq!(p.midi_tracks[0].active.len(), 1, "mute must not cut notes");
    assert_eq!(p.midi_tracks[1].active.len(), 1);

    p.update_track_mute("track-1", false);
    p.update_track_solo("track-2", true);
    assert_eq!(
        p.midi_tracks[0].active.len(),
        1,
        "soloing another track must not cut this one's notes"
    );
    assert_eq!(p.midi_tracks[1].active.len(), 1);

    p.update_track_solo("track-2", false);
    assert_eq!(p.midi_tracks[0].active.len(), 1);
    assert_eq!(p.midi_tracks[1].active.len(), 1);
}

#[test]
fn all_notes_off_clears_preview_tracker() {
    // A held preview/audition note that never received an explicit note-off
    // (e.g. deleted mid-move) must not leave the engine believing a note is
    // still sounding — the panic clears the preview tracker.
    let mut p = project_with(vec![clip_with_one_note()]);
    let track_id = p.midi_tracks[0].track_id.clone();
    p.midi_tracks[0].preview_active.push((0, 60));
    assert!(p.has_active_midi_preview());
    p.midi_preview_all_notes_off(&track_id);
    assert!(p.midi_tracks[0].preview_active.is_empty());
    assert!(!p.has_active_midi_preview());
}

#[test]
fn tempo_change_reschedules_midi_samples_from_beats() {
    let mut p = project_with(vec![clip_with_one_note()]);
    let next_pos = p.set_static_midi_tempo(60.0, 96_000);
    let evs = &p.midi_tracks[0].events;
    let on = evs
        .iter()
        .find(|e| e.kind == RuntimeMidiEventKind::NoteOn)
        .unwrap();
    let off = evs
        .iter()
        .find(|e| e.kind == RuntimeMidiEventKind::NoteOff)
        .unwrap();

    // 60 BPM @ 48 kHz -> 48000 samples/beat. The note stays at beat 4..5,
    // so only its sample positions change.
    assert_eq!(on.beat, 4.0);
    assert_eq!(off.beat, 5.0);
    assert_eq!(on.sample, 192_000);
    assert_eq!(off.sample, 240_000);
    // Current sample 96000 was beat 4 at 120 BPM; preserve beat 4.
    assert_eq!(next_pos, 192_000);
}

#[test]
fn controller_points_resolve_to_control_change_events() {
    let mut clip = clip_with_one_note();
    clip.controllers = vec![EngineMidiControllerLane {
        controller: 11,
        channel: 0,
        points: vec![
            EngineMidiControllerPoint {
                beat: 0.0,
                value: 0.25,
            },
            EngineMidiControllerPoint {
                beat: 2.0,
                value: 0.75,
            },
        ],
    }];
    let p = project_with(vec![clip]);
    let cc: Vec<&RuntimeMidiEvent> = p.midi_tracks[0]
        .events
        .iter()
        .filter(|e| e.kind == RuntimeMidiEventKind::ControlChange)
        .collect();
    assert_eq!(cc.len(), 2);
    // First point: abs beat 4.0 → 96000 sa, cc 11, value 0.25.
    assert_eq!(cc[0].cc_number, 11);
    assert_eq!(cc[0].sample, 96_000);
    assert!((cc[0].cc_value - 0.25).abs() < 1e-6);
    // Second point: abs beat 6.0 → 144000 sa, value 0.75.
    assert_eq!(cc[1].sample, 144_000);
    assert!((cc[1].cc_value - 0.75).abs() < 1e-6);
}

#[test]
fn control_change_does_not_affect_active_notes() {
    let mut clip = clip_with_one_note();
    clip.controllers = vec![EngineMidiControllerLane {
        controller: 1,
        channel: 0,
        points: vec![EngineMidiControllerPoint {
            beat: 0.0,
            value: 0.5,
        }],
    }];
    let mut p = project_with(vec![clip]);
    p.reset_midi_playback(0);
    // Block covering the CC at abs beat 4.0 (96000) but the note also starts
    // there — active set should track only the note, not the CC.
    p.schedule_midi_block(96_000, 512);
    assert_eq!(p.midi_tracks[0].active, vec![(0u8, 60u8)]);
}

#[test]
fn automation_points_are_sorted_and_clamped_for_runtime() {
    let lane = RuntimeAutomationLane::from_snapshot(&EngineAutomationLaneSnapshot {
        id: "lane-1".into(),
        name: "Volume".into(),
        target: crate::types::EngineAutomationTargetSnapshot {
            tag: 0,
            ..Default::default()
        },
        enabled: true,
        points: vec![
            crate::types::EngineAutomationPointSnapshot {
                beat: 4.0,
                value: 2.0,
                curve: 0,
                tension: 0.0,
            },
            crate::types::EngineAutomationPointSnapshot {
                beat: -1.0,
                value: -0.5,
                curve: 1,
                tension: 0.0,
            },
        ],
    });

    assert_eq!(lane.points[0].beat, 0.0);
    assert_eq!(lane.points[0].value, 0.0);
    assert_eq!(lane.points[1].beat, 4.0);
    assert_eq!(lane.points[1].value, 1.0);
}

#[test]
fn automation_evaluator_handles_linear_and_hold_curves() {
    let points = vec![
        RuntimeAutomationPoint {
            beat: 0.0,
            value: 0.0,
            curve: RuntimeAutomationCurve::Linear,
            tension: 0.0,
        },
        RuntimeAutomationPoint {
            beat: 4.0,
            value: 1.0,
            curve: RuntimeAutomationCurve::Hold,
            tension: 0.0,
        },
        RuntimeAutomationPoint {
            beat: 8.0,
            value: 0.25,
            curve: RuntimeAutomationCurve::Linear,
            tension: 0.0,
        },
    ];

    assert_eq!(evaluate_automation_points(&[], 2.0, 0.75), 0.75);
    assert!((evaluate_automation_points(&points, 2.0, 0.5) - 0.5).abs() < 1e-6);
    assert!((evaluate_automation_points(&points, 6.0, 0.5) - 1.0).abs() < 1e-6);
    assert!((evaluate_automation_points(&points, 10.0, 0.5) - 0.25).abs() < 1e-6);
}

#[test]
fn automation_curve_factor_shapes_match_spec() {
    // Linear (tension 0): identity.
    assert!((automation_curve_factor(0, 0.0, 0.5) - 0.5).abs() < 1e-6);
    // Hold: always 0 (value stays at the left point).
    assert_eq!(automation_curve_factor(1, 0.0, 0.5), 0.0);
    assert_eq!(automation_curve_factor(1, 0.0, 0.99), 0.0);
    // Smooth S-curve: symmetric about the midpoint, passes through 0.5 at t=0.5.
    assert!((automation_curve_factor(2, 0.0, 0.5) - 0.5).abs() < 1e-6);
    let lo = automation_curve_factor(2, 0.0, 0.25);
    let hi = automation_curve_factor(2, 0.0, 0.75);
    assert!(lo < 0.25 && hi > 0.75, "smoothstep eases both ends");
    assert!((lo + hi - 1.0).abs() < 1e-6, "smoothstep is symmetric");
    // Positive tension eases in (below the linear line mid-segment); negative
    // tension eases out (above it). They mirror around the linear line.
    let ease_in = automation_curve_factor(0, 1.0, 0.5);
    let ease_out = automation_curve_factor(0, -1.0, 0.5);
    assert!(ease_in < 0.5, "positive tension is exponential/ease-in");
    assert!(ease_out > 0.5, "negative tension is logarithmic/ease-out");
    // +k and -k are reflections across the diagonal (inverse power curves):
    // ease_out(ease_in(t)) == t.
    let reflected = automation_curve_factor(0, -1.0, automation_curve_factor(0, 1.0, 0.3));
    assert!(
        (reflected - 0.3).abs() < 1e-5,
        "ease-in/out are mirror curves"
    );
    // Endpoints are always pinned regardless of shape/tension.
    for tag in [0u8, 1, 2] {
        for tension in [-1.0f32, -0.3, 0.0, 0.6, 1.0] {
            assert_eq!(automation_curve_factor(tag, tension, 0.0), 0.0);
            if tag != 1 {
                assert!((automation_curve_factor(tag, tension, 1.0) - 1.0).abs() < 1e-6);
            }
        }
    }
}

/// An enabled lane with no points must not speak for the channel.
///
/// `automation_values_at_beat` feeds `apply_fader`, which prefers the lane
/// over `RuntimeTrack::volume`. An empty Track Volume lane answering with its
/// 0 dB default therefore discarded every `SetTrackVolume` the fader sent —
/// the command applied, the value was simply never read. Pan and Mute lanes
/// pinned centre and un-muted the same way.
#[test]
fn an_empty_track_lane_leaves_the_fader_in_charge() {
    let lane = |target: RuntimeAutomationTarget| RuntimeAutomationLane {
        id: "lane".into(),
        name: "Lane".into(),
        target,
        enabled: true,
        points: Vec::new(),
    };
    for target in [
        RuntimeAutomationTarget::TrackVolume,
        RuntimeAutomationTarget::TrackPan,
        RuntimeAutomationTarget::TrackMute,
    ] {
        assert!(
            lane(target.clone()).evaluate_normalized(0.0).is_none(),
            "an empty {target:?} lane must not produce a value"
        );
    }

    // Through the real snapshot → runtime path, on the track the fader talks
    // to. `automation_values_at_beat` is what `apply_fader` consults.
    let mut track = automation_track_snapshot(vec![
        empty_lane_snapshot("vol", 0),
        empty_lane_snapshot("pan", 1),
        empty_lane_snapshot("mute", 2),
    ]);
    track.volume = 0.2818; // -11 dB, as sent by SetTrackVolume
    let runtime = automation_runtime(track);
    let values = runtime.tracks[0].automation_values_at_beat(0.0);
    assert_eq!(values.volume, None, "the fader must survive an empty lane");
    assert_eq!(values.pan, None);
    assert_eq!(values.muted, None);
    assert!((runtime.tracks[0].volume - 0.2818).abs() < 1.0e-6);
}

/// Read mode is unchanged: a lane that actually carries points still drives
/// the channel and still overrides the fader.
#[test]
fn a_lane_with_points_still_overrides_the_fader() {
    let mut lane = empty_lane_snapshot("vol", 0);
    lane.points = vec![crate::types::EngineAutomationPointSnapshot {
        beat: 0.0,
        value: volume_db_to_norm(-11.0),
        curve: 0,
        tension: 0.0,
    }];
    let mut track = automation_track_snapshot(vec![lane]);
    track.volume = 1.0;
    let runtime = automation_runtime(track);
    let volume = runtime.tracks[0]
        .automation_values_at_beat(0.0)
        .volume
        .expect("a lane with points drives the channel");
    let expected = 10.0f32.powf(-11.0 / 20.0);
    assert!(
        (volume - expected).abs() < 1.0e-3,
        "expected {expected:.4} from the lane, got {volume:.4}"
    );
}

fn empty_lane_snapshot(id: &str, tag: u8) -> EngineAutomationLaneSnapshot {
    EngineAutomationLaneSnapshot {
        id: id.to_string(),
        name: id.to_string(),
        target: crate::types::EngineAutomationTargetSnapshot {
            tag,
            ..Default::default()
        },
        enabled: true,
        points: Vec::new(),
    }
}

fn automation_track_snapshot(
    automation_lanes: Vec<EngineAutomationLaneSnapshot>,
) -> crate::types::EngineTrackSnapshot {
    crate::types::EngineTrackSnapshot {
        midi_programs: Vec::new(),
        id: "track-1".to_string(),
        track_type: "audio".to_string(),
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
        automation_lanes,
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

/// Two notes of one pitch that overlap: the later retriggers the key, and
/// only the last note-off releases it, so the first note's off can no
/// longer silence the note meant to be sounding.
#[test]
fn overlapping_notes_of_one_pitch_retrigger_instead_of_cutting() {
    let ev = |sample: u64, kind: RuntimeMidiEventKind, pitch: u8| RuntimeMidiEvent {
        sample,
        beat: 0.0,
        kind,
        pitch,
        velocity: 100,
        channel: 0,
        note_id: 0,
        cc_number: 0,
        cc_value: 0.0,
        pitch_hz: 0.0,
        source: None,
    };
    use RuntimeMidiEventKind::{NoteOff, NoteOn};
    // C held 0..300; C again 100..200; E 0..300 untouched.
    let mut events = vec![
        ev(0, NoteOn, 60),
        ev(0, NoteOn, 64),
        ev(100, NoteOn, 60),
        ev(200, NoteOff, 60),
        ev(300, NoteOff, 60),
        ev(300, NoteOff, 64),
    ];
    sort_midi_events(&mut events);
    let resolved = resolve_overlapping_notes(events);
    let shape: Vec<(u64, bool, u8)> = resolved
        .iter()
        .map(|e| (e.sample, e.kind == NoteOn, e.pitch))
        .collect();
    assert_eq!(
        shape,
        [
            (0, true, 60),
            (0, true, 64),
            (100, false, 60),
            (100, true, 60),
            (300, false, 60),
            (300, false, 64),
        ]
    );
}

fn automation_runtime(track: crate::types::EngineTrackSnapshot) -> RuntimeProject {
    let snapshot = EngineProjectSnapshot {
        spatial: Default::default(),
        project_id: "automation-lane".to_string(),
        project_root: None,
        preferred_input_device: None,
        bpm: 120.0,
        tempo_points: Vec::new(),
        time_signature: [4, 4],
        sample_rate: 48_000,
        tracks: vec![track],
        clips: Vec::new(),
        midi_clips: Vec::new(),
        pdc_enabled: true,
        latency_graph_version: 1,
        routing: crate::types::EngineRoutingSnapshot {
            master_output_device: None,
            sample_rate: 48_000,
            buffer_size: 256,
        },
    };
    RuntimeProject::build(&snapshot, 48_000, &mut HashMap::new(), None, true)
        .expect("automation runtime")
}

/// A send-level lane in a project snapshot reaches the built runtime bound
/// to its send, and reads there.
#[test]
fn a_snapshot_send_lane_binds_to_its_send_and_reads() {
    let mut lane = empty_lane_snapshot("send-lane", 4);
    lane.target.send_id = "send-1".to_string();
    lane.points = vec![crate::types::EngineAutomationPointSnapshot {
        beat: 0.0,
        value: volume_db_to_norm(-12.0),
        curve: 0,
        tension: 0.0,
    }];
    let mut track = automation_track_snapshot(vec![lane]);
    track.sends = vec![crate::types::EngineSendSnapshot {
        id: "send-1".to_string(),
        return_track_id: "ret-1".to_string(),
        level: 1.0,
        enabled: true,
        pre_fader: false,
        wdw: false,
    }];
    let runtime = automation_runtime(track);
    let src = runtime
        .tracks
        .iter()
        .find(|t| t.id == "track-1")
        .expect("track");
    assert_eq!(
        src.sends[0].level_lane,
        Some(0),
        "the lane is bound to its send"
    );
    let (_, level) = src.send_state_at(0, 2.0);
    let expected = volume_norm_to_linear(volume_db_to_norm(-12.0));
    assert!((level - expected).abs() < 1e-5, "{level} vs {expected}");
}

#[test]
fn automation_evaluator_applies_tension() {
    // A single curved segment 0→1 over beats [0, 4]. With ease-in tension the
    // mid-segment value sits below the linear midpoint; ease-out sits above.
    let curved = |tension: f32| {
        vec![
            RuntimeAutomationPoint {
                beat: 0.0,
                value: 0.0,
                curve: RuntimeAutomationCurve::Linear,
                tension,
            },
            RuntimeAutomationPoint {
                beat: 4.0,
                value: 1.0,
                curve: RuntimeAutomationCurve::Linear,
                tension: 0.0,
            },
        ]
    };
    let mid_linear = evaluate_automation_points(&curved(0.0), 2.0, 0.0);
    let mid_ease_in = evaluate_automation_points(&curved(1.0), 2.0, 0.0);
    let mid_ease_out = evaluate_automation_points(&curved(-1.0), 2.0, 0.0);
    assert!((mid_linear - 0.5).abs() < 1e-6);
    assert!(mid_ease_in < mid_linear);
    assert!(mid_ease_out > mid_linear);
}

#[test]
fn disabled_or_unresolved_automation_lanes_do_not_evaluate() {
    let mut lane = RuntimeAutomationLane::from_snapshot(&EngineAutomationLaneSnapshot {
        id: "lane-1".into(),
        name: "Missing Param".into(),
        target: crate::types::EngineAutomationTargetSnapshot {
            tag: 3,
            ..Default::default()
        },
        enabled: true,
        points: vec![crate::types::EngineAutomationPointSnapshot {
            beat: 0.0,
            value: 0.25,
            curve: 0,
            tension: 0.0,
        }],
    });
    assert!(lane.evaluate_normalized(0.0).is_none());

    lane.target = RuntimeAutomationTarget::TrackPan;
    lane.enabled = false;
    assert!(lane.evaluate_normalized(0.0).is_none());
}

#[test]
fn build_plugin_param_bindings_resolves_only_valid_lanes() {
    let point = || RuntimeAutomationPoint {
        beat: 0.0,
        value: 0.5,
        curve: RuntimeAutomationCurve::Linear,
        tension: 0.0,
    };
    let lane = |id: &str,
                insert: &str,
                param: &str,
                enabled: bool,
                points: Vec<RuntimeAutomationPoint>| {
        RuntimeAutomationLane {
            id: id.to_string(),
            name: "p".to_string(),
            target: RuntimeAutomationTarget::PluginParameter {
                insert_id: insert.to_string(),
                parameter_id: param.to_string(),
            },
            enabled,
            points,
        }
    };
    let mut track = bridged_instrument_track("track-1");
    track.automation_lanes = vec![
        // valid → resolves to insert_ix 0, param 42
        lane("l0", "insert-1", "42", true, vec![point()]),
        // missing insert → skipped
        lane("l1", "insert-missing", "7", true, vec![point()]),
        // disabled → skipped
        lane("l2", "insert-1", "8", false, vec![point()]),
        // empty points → skipped (would otherwise force the default value)
        lane("l3", "insert-1", "9", true, Vec::new()),
        // non-numeric param id → skipped
        lane("l4", "insert-1", "cutoff", true, vec![point()]),
    ];

    let bindings = build_plugin_param_bindings(&track);
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].insert_ix, 0);
    assert_eq!(bindings[0].lane_ix, 0);
    assert_eq!(bindings[0].param_id, 42);
    assert!(bindings[0].last_value.is_nan());
}

// ── VSTi bridge MIDI tests ───────────────────────────────────────────────

/// Test sink recording every `push_midi` call as (status, data1, data2, offset).
#[derive(Debug, Default)]
struct RecordingSink {
    events: std::sync::Mutex<Vec<(u8, u8, u8, u32)>>,
}

impl RecordingSink {
    fn take(&self) -> Vec<(u8, u8, u8, u32)> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

impl crate::plugin_bridge::PluginBridgeSink for RecordingSink {
    fn dsp_ready(&self) -> bool {
        true
    }
    fn read_output(&self, _out_l: &mut [f32], _out_r: &mut [f32], _frames: usize) -> usize {
        0
    }
    fn push_midi(&self, status: u8, data1: u8, data2: u8, sample_offset: u32) {
        self.events
            .lock()
            .unwrap()
            .push((status, data1, data2, sample_offset));
    }
    fn write_input(&self, _in_l: &[f32], _in_r: &[f32], _frames: usize) {}
    fn request_block(&self, _frames: u32) {}
}

fn bridged_instrument_track(id: &str) -> RuntimeTrack {
    RuntimeTrack {
        spatial: None,
        spatial_params: Default::default(),
        active_voices: 0,
        listen: crate::monitor::ListenMode::Off,
        id: id.to_string(),
        track_type: "midi".to_string(),
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        record_armed: false,
        monitor_enabled: false,
        input_source: RuntimeTrackInputSource::None,
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
            kind_tag: RuntimeInsertKind::ExternalBridge,
            enabled: true,
            bypass_lane: None,
            automation_bypassed: false,
            params: std::sync::Arc::new(HashMap::new()),
            bridge_is_effect: false,
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
            scratch_l: vec![0.0; 64],
            scratch_r: vec![0.0; 64],
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
        block_l: vec![0.0; 64],
        block_r: vec![0.0; 64],
        recv_l: vec![0.0; 64],
        recv_r: vec![0.0; 64],
        soundfont_l: vec![0.0; 64],
        soundfont_r: vec![0.0; 64],
        ara_renderers: Vec::new(),
        ara_l: vec![0.0; 64],
        ara_r: vec![0.0; 64],
        ara_silence: vec![0.0; 64],
        ara_latency_samples: 0,
        midi_block_events: Vec::new(),
        solfege_pitch_events: Vec::new(),
        solfege_articulation_events: Vec::new(),
        midi_instrument_insert_ix: Some(0),
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

/// A track's bank and program reach its instrument once, in bank-first
/// order — not again on every block, rebuilt graph or restart, since a
/// program change makes some instruments reload — and again when the
/// plug-in behind the track is replaced.
#[test]
fn a_program_selection_is_sent_once_per_instrument() {
    use crate::plugin_bridge::PluginBridgeSink;
    let selection = RuntimeMidiProgram {
        track_id: "track-1".to_string(),
        track_index: 0,
        program: crate::types::EngineMidiProgram {
            channel: 2,
            bank_msb: Some(127),
            bank_lsb: Some(0),
            program: 25,
        },
        sent_to: None,
    };
    let mut p = project_with(vec![]);
    p.tracks = vec![bridged_instrument_track("track-1")];
    p.midi_programs = vec![selection.clone()];

    // Still loading: no sink, so nothing is sent and nothing marked sent.
    p.flush_midi_programs();
    assert_eq!(p.midi_programs[0].sent_to, None);

    let sink = Arc::new(RecordingSink::default());
    p.tracks[0].inserts[0].bridge_sink = Some(sink.clone() as Arc<dyn PluginBridgeSink>);
    p.flush_midi_programs();
    assert_eq!(
        sink.take(),
        vec![(0xB2, 0, 127, 0), (0xB2, 32, 0, 0), (0xC2, 25, 0, 0)]
    );
    p.flush_midi_programs();
    assert!(sink.take().is_empty(), "sent again on the next block");

    let mut next = project_with(vec![]);
    next.tracks = vec![bridged_instrument_track("track-1")];
    next.tracks[0].inserts[0].bridge_sink = Some(sink.clone() as Arc<dyn PluginBridgeSink>);
    next.midi_programs = vec![selection];
    next.inherit_midi_programs_sent(&p);
    next.flush_midi_programs();
    assert!(sink.take().is_empty(), "sent again after a graph swap");

    let reloaded = Arc::new(RecordingSink::default());
    next.tracks[0].inserts[0].bridge_sink = Some(reloaded.clone() as Arc<dyn PluginBridgeSink>);
    next.flush_midi_programs();
    assert_eq!(reloaded.take().len(), 3, "a reloaded plug-in is not told");
}

#[test]
fn vsti_output_children_scatter_demuxes_channel_pairs() {
    use crate::engine::scatter_vsti_output_children;
    let frames = 2usize;
    let channels = 4usize;

    let mut p = project_with(vec![]);
    // Source bridged instrument with a child route: plugin ch 3/4 -> "out-3".
    let mut inst = bridged_instrument_track("track-1");
    inst.inserts[0].vsti_output_children = vec![RuntimeVstiOutputChild {
        dest_track_id: "out-3".to_string(),
        dest_track_index: None,
        bus_index: 1,
        channel_count: 2,
        channel_l: 3,
        channel_r: 4,
    }];
    // Interleaved 4-ch block read by the engine: ch1/2 are bus 0 L/R,
    // ch3/4 are bus 1 L/R. These are audio channels, not drum-piece names.
    inst.inserts[0].scratch_multi = vec![
        10.0, 20.0, 3.0, 4.0, // frame 0
        11.0, 21.0, 5.0, 6.0, // frame 1
    ];
    assert_eq!(inst.inserts[0].scratch_multi.len() / frames, channels);
    p.tracks.push(inst);

    // Destination "Out Ch" track (routing-style); recv starts zeroed.
    let mut dest = bridged_instrument_track("out-3");
    dest.track_type = "bus".to_string();
    dest.inserts.clear();
    p.tracks.push(dest);

    p.resolve_indices();
    let src_idx = p.tracks.iter().position(|t| t.id == "track-1").unwrap();
    let mut master = vec![0.0f32; frames * 2];
    scatter_vsti_output_children(&mut p, src_idx, frames, &mut master, 2);

    let dest_idx = p.tracks.iter().position(|t| t.id == "out-3").unwrap();
    // The child strip received plugin channels 3/4, not the main 1/2.
    assert_eq!(p.tracks[dest_idx].recv_l[0], 3.0);
    assert_eq!(p.tracks[dest_idx].recv_r[0], 4.0);
    assert_eq!(p.tracks[dest_idx].recv_l[1], 5.0);
    assert_eq!(p.tracks[dest_idx].recv_r[1], 6.0);
    assert_eq!(master, vec![0.0; frames * 2]);
}

#[test]
fn vsti_output_child_missing_destination_falls_back_to_master() {
    use crate::engine::scatter_vsti_output_children;
    let frames = 2usize;
    let mut p = project_with(vec![]);
    let mut inst = bridged_instrument_track("track-1");
    inst.inserts[0].vsti_output_children = vec![RuntimeVstiOutputChild {
        dest_track_id: "missing-out".to_string(),
        dest_track_index: None,
        bus_index: 1,
        channel_count: 2,
        channel_l: 3,
        channel_r: 4,
    }];
    inst.inserts[0].scratch_multi = vec![
        0.0, 0.0, 0.25, 0.5, // frame 0
        0.0, 0.0, 0.75, 1.0, // frame 1
    ];
    p.tracks.push(inst);
    p.resolve_indices();

    let src_idx = p.tracks.iter().position(|t| t.id == "track-1").unwrap();
    let mut master = vec![0.0f32; frames * 2];
    scatter_vsti_output_children(&mut p, src_idx, frames, &mut master, 2);

    assert_eq!(master, vec![0.25, 0.5, 0.75, 1.0]);
}

/// Project with the one-note clip on a bridged instrument track plus a
/// recording sink installed as its plugin-bridge sink.
fn bridged_project() -> (RuntimeProject, Arc<RecordingSink>) {
    let mut p = project_with(vec![clip_with_one_note()]);
    p.tracks.push(bridged_instrument_track("track-1"));
    let sink = Arc::new(RecordingSink::default());
    p.plugin_bridge_sinks
        .insert("insert-1".to_string(), sink.clone());
    // Mirror the engine: indices + cached bridge sinks are resolved before
    // the block path runs.
    p.resolve_indices();
    (p, sink)
}

#[test]
fn scheduled_bridge_events_carry_offset_velocity_and_channel() {
    let (mut p, sink) = bridged_project();
    p.reset_midi_playback(95_880);
    sink.take(); // discard the seek panic CCs

    // NoteOn at absolute sample 96_000 inside block 95_880..96_392.
    p.schedule_midi_block(95_880, 512);
    assert_eq!(sink.take(), vec![(0x90, 60, 100, 120)]);

    // NoteOff at 120_000 inside block 119_900..120_412.
    p.schedule_midi_block(119_900, 512);
    assert_eq!(sink.take(), vec![(0x80, 60, 0, 100)]);
}

#[test]
fn bridge_encodes_mpe_pressure_and_pitch_as_midi_status_bytes() {
    let sink = RecordingSink::default();
    push_vst3_midi_event_to_sink(
        &sink,
        &Vst3MidiEvent::control_change(7, 2, 128, 0.75),
        "insert-1",
        false,
    );
    push_vst3_midi_event_to_sink(
        &sink,
        &Vst3MidiEvent::control_change(8, 2, 129, 0.5),
        "insert-1",
        false,
    );
    assert_eq!(
        sink.take(),
        vec![(0xD0 | 2, 95, 0, 7), (0xE0 | 2, 0, 64, 8)]
    );
}

/// The schedule a note edit swaps in, resolved to the one bridged track.
fn schedule_of(clips: Vec<EngineMidiClipSnapshot>) -> RuntimeMidiData {
    let tempo_map = RuntimeTempoMapSnapshot::static_tempo(120.0);
    let (midi_clips, mut midi_tracks) =
        build_midi_runtime(&shared_midi_clips(&clips), &tempo_map, 48_000);
    for track in &mut midi_tracks {
        track.track_index = Some(0);
    }
    RuntimeMidiData {
        midi_clips,
        midi_tracks,
    }
}

/// Drawing a note while another one sounds must not cut it: the edit
/// swaps the schedule and nothing is released or panicked.
#[test]
fn a_note_edit_leaves_the_sounding_note_alone() {
    let (mut p, sink) = bridged_project();
    p.reset_midi_playback(95_880);
    p.schedule_midi_block(95_880, 512);
    sink.take();
    assert_eq!(p.midi_tracks[0].active, vec![(0, 60)]);

    let mut clip = clip_with_one_note();
    let mut added = clip.notes[0].clone();
    added.id = 2;
    added.pitch = 64;
    added.start_beat = 2.0;
    clip.notes.push(added);
    let mut next = schedule_of(vec![clip]);
    p.replace_midi_schedule(&mut next, 96_392);

    assert!(
        sink.take().is_empty(),
        "the edit sent MIDI to the instrument"
    );
    assert_eq!(p.midi_tracks[0].active, vec![(0, 60)]);
    // The old schedule is handed back for retiring.
    assert_eq!(next.midi_tracks[0].events.len(), 2);

    // The sounding note still ends where it did, and the new one plays.
    p.schedule_midi_block(119_900, 512);
    assert_eq!(sink.take(), vec![(0x80, 60, 0, 100)]);
    p.schedule_midi_block(143_900, 512);
    assert_eq!(sink.take(), vec![(0x90, 64, 100, 100)]);
}

/// Deleting the note that is sounding releases it at once: its note-off
/// left the schedule with it.
#[test]
fn deleting_a_sounding_note_releases_it() {
    let (mut p, sink) = bridged_project();
    p.reset_midi_playback(95_880);
    p.schedule_midi_block(95_880, 512);
    sink.take();

    let mut clip = clip_with_one_note();
    clip.notes.clear();
    let mut next = schedule_of(vec![clip]);
    p.replace_midi_schedule(&mut next, 96_392);

    assert_eq!(sink.take(), vec![(0x80, 60, 0, 0)]);
    assert!(p.midi_tracks.iter().all(|track| track.active.is_empty()));
}

#[test]
fn loop_wrap_bridge_events_keep_callback_offset() {
    let (mut p, sink) = bridged_project();
    p.reset_midi_playback(119_900);
    sink.take(); // discard the seek panic CCs

    let end_reset = crate::engine::schedule_midi_render_block(
        &mut p,
        119_900,
        300,
        Some(crate::transport::LoopBounds {
            start: 96_000,
            end: 120_000,
        }),
    );

    assert!(end_reset.is_none());
    let events = sink.take();
    assert!(
        events.contains(&(0x90, 60, 100, 100)),
        "wrapped NoteOn should land 100 samples into the callback: {events:?}"
    );
}

#[test]
fn stop_panic_pushes_note_offs_and_ccs_and_arms_bridge_flush() {
    let (mut p, sink) = bridged_project();
    p.reset_midi_playback(0);
    p.schedule_midi_block(96_000, 512); // fires the NoteOn
    assert_eq!(p.midi_tracks[0].active, vec![(0u8, 60u8)]);
    sink.take();

    p.all_notes_off("stop");

    let events = sink.take();
    // The tracked active note is released explicitly, first.
    assert_eq!(events[0], (0x80, 60, 0, 0));
    // Then Sustain Off / All Notes Off / All Sound Off on every channel.
    for ch in 0u8..16 {
        assert!(
            events.contains(&(0xB0 | ch, 64, 0, 0)),
            "sustain off ch={ch}"
        );
        assert!(
            events.contains(&(0xB0 | ch, 123, 0, 0)),
            "all notes off ch={ch}"
        );
        assert!(
            events.contains(&(0xB0 | ch, 120, 0, 0)),
            "all sound off ch={ch}"
        );
    }
    assert!(p.midi_tracks[0].active.is_empty());
}

#[test]
fn repeated_bridge_preview_cycle_leaves_no_stuck_notes() {
    let (mut p, sink) = bridged_project();
    for _ in 0..2 {
        p.bridge_preview_note_on("track-1", "insert-1", 0, 64, 110);
        assert!(p.has_active_midi_preview());
        p.bridge_preview_note_off("track-1", "insert-1", 0, 64);
        assert!(!p.has_active_midi_preview());
    }
    assert_eq!(
        sink.take(),
        vec![
            (0x90, 64, 110, 0),
            (0x80, 64, 0, 0),
            (0x90, 64, 110, 0),
            (0x80, 64, 0, 0),
        ]
    );
}

/// The release tail needs no window of its own: the graph renders every
/// block the engine renders, so a bridged instrument's release is still
/// being processed long after the note-off, editor open or not.
#[test]
fn bridge_preview_note_off_reaches_the_host_without_editor() {
    let (mut p, sink) = bridged_project();
    p.bridge_preview_note_on("track-1", "insert-1", 0, 67, 96);
    sink.take();

    p.bridge_preview_note_off("track-1", "insert-1", 0, 67);

    assert_eq!(sink.take(), vec![(0x80, 67, 0, 0)]);
    assert!(!p.has_active_midi_preview());
}

#[test]
fn preview_all_notes_off_releases_held_notes() {
    let (mut p, sink) = bridged_project();
    p.bridge_preview_note_on("track-1", "insert-1", 0, 72, 90);
    sink.take();

    p.bridge_preview_all_notes_off("track-1", "insert-1");

    let events = sink.take();
    assert_eq!(events[0], (0x80, 72, 0, 0));
    assert!(events.contains(&(0xB0, 123, 0, 0)));
    assert!(!p.has_active_midi_preview());
}

/// A keyboard bend used to arrive as CC 127 (Poly Mode On): controller
/// 129 was clamped into the CC range before it reached the sink.
#[test]
fn bridge_preview_pitch_bend_and_pressure_keep_their_status_bytes() {
    let (mut p, sink) = bridged_project();

    p.bridge_preview_pitch_bend("track-1", "insert-1", 2, 0x3FFF);
    p.bridge_preview_pitch_bend("track-1", "insert-1", 2, 8192);
    p.bridge_preview_control_change("track-1", "insert-1", 2, 129, 64);
    p.bridge_preview_control_change("track-1", "insert-1", 2, 128, 90);

    assert_eq!(
        sink.take(),
        vec![
            (0xE2, 0x7F, 0x7F, 0),
            (0xE2, 0x00, 0x40, 0),
            (0xE2, 0x00, 0x40, 0),
            (0xD2, 90, 0, 0),
        ]
    );
}
