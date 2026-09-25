use super::*;

#[cfg(test)]
mod instrument_lifecycle_tests {
    use super::*;

    fn instrument_track(state: &mut TimelineState) -> String {
        state.tracks.clear();
        state.create_track(CreateTrackOptions {
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
            input_monitor: InputMonitorMode::Off,
        })
    }

    fn load_vsti(
        state: &mut TimelineState,
        track_id: &str,
        slot_index: usize,
        class_id: &str,
        path: &str,
    ) -> String {
        let slot = state
            .ensure_insert_slot_at(track_id, slot_index)
            .expect("slot");
        state.set_insert_plugin(
            track_id,
            &slot,
            class_id.to_string(),
            Some(std::path::PathBuf::from(path)),
            InsertPluginFormat::Vst3,
            None,
            class_id.to_string(),
        );
        slot
    }

    /// Test 1 (model half): removing a VSTi clears the slot and the canonical
    /// instrument instance pointer.
    #[test]
    fn remove_vsti_clears_slot_and_instrument_pointer() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot = load_vsti(&mut state, &track_id, 0, "synth", "C:/p/synth.vst3");

        let track = state.find_track(&track_id).unwrap();
        assert_eq!(
            track.instrument_plugin_instance_id.as_deref(),
            Some(slot.as_str())
        );

        state.remove_insert(&track_id, &slot);
        let track = state.find_track(&track_id).unwrap();
        assert!(track.inserts.is_empty(), "slot must be gone");
        assert!(
            track.instrument_plugin_instance_id.is_none(),
            "instrument pointer must be cleared"
        );
    }

    /// Test 2: add VSTi A, remove it, add VSTi B → B gets a brand-new instance
    /// id (the old one is never reused, so the engine cannot resurrect A).
    #[test]
    fn re_add_after_remove_gets_fresh_instance_id() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot_a = load_vsti(&mut state, &track_id, 0, "synth-a", "C:/p/a.vst3");
        state.remove_insert(&track_id, &slot_a);
        let slot_b = load_vsti(&mut state, &track_id, 0, "synth-b", "C:/p/b.vst3");
        assert_ne!(slot_a, slot_b, "re-added VSTi must get a fresh instance id");
        assert_eq!(
            state
                .find_track(&track_id)
                .unwrap()
                .instrument_plugin_instance_id
                .as_deref(),
            Some(slot_b.as_str())
        );
    }

    #[test]
    fn detected_vsti_outputs_auto_enable_for_fresh_slot_only() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot = load_vsti(&mut state, &track_id, 0, "drums", "C:/p/drums.vst3");

        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 8));
        let channels = state
            .find_insert_slot(&track_id, &slot)
            .unwrap()
            .enabled_audio_output_channels
            .clone();
        assert_eq!(channels, vec![1, 2, 3, 4, 5, 6, 7, 8]);

        assert!(!state.auto_enable_detected_insert_outputs(&track_id, &slot, 16));
        let channels = state
            .find_insert_slot(&track_id, &slot)
            .unwrap()
            .enabled_audio_output_channels
            .clone();
        assert_eq!(channels, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// VSTi multi-out child channels are mixer-only: they live in `state.tracks`
    /// (so the engine snapshot/mixer can route + meter them) but must never take
    /// up arrangement space — zero row height, excluded from the timeline's
    /// scrollable height, and never hit-tested as an arrangement row. The rows
    /// vector stays 1:1 with `state.tracks` so the timeline's
    /// `row.index == state.tracks position` invariant is preserved.
    #[test]
    fn vsti_output_child_channels_are_excluded_from_arrangement_layout() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot = load_vsti(&mut state, &track_id, 0, "drums", "C:/p/drums.vst3");

        // A declared 4-bus layout creates 4 child mixer channels (buses 0..=3).
        state.set_insert_output_bus_layout(&track_id, &slot, &[2, 2, 2, 2]);
        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 8));

        let child_count = state
            .tracks
            .iter()
            .filter(|t| is_vsti_output_child_track_id(&t.id))
            .count();
        assert_eq!(child_count, 4, "one mixer-only strip per stereo output bus");

        let layout = state.track_row_layout();
        // 1:1 with state.tracks → arrangement index invariant intact.
        assert_eq!(layout.rows.len(), state.tracks.len());

        let parent_height = layout
            .rows
            .iter()
            .find(|r| r.track_id == track_id)
            .map(|r| r.height)
            .expect("parent instrument row");
        assert!(parent_height > 0.0);
        for row in &layout.rows {
            if is_vsti_output_child_track_id(&row.track_id) {
                assert_eq!(
                    row.height, 0.0,
                    "child channel {} must not occupy timeline space",
                    row.track_id
                );
            }
        }
        assert_eq!(
            layout.total_height, parent_height,
            "child channels must not contribute to arrangement height"
        );

        // Nothing is hit-tested below the single visible instrument track.
        assert!(
            layout.track_at_content_y(parent_height + 1.0).is_none(),
            "child channels must not be hit-testable as arrangement rows"
        );
    }

    #[test]
    fn detected_outputs_without_multibus_layout_do_not_create_child_channels() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot = load_vsti(&mut state, &track_id, 0, "synth", "C:/p/synth.vst3");

        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 8));
        assert_eq!(
            state
                .tracks
                .iter()
                .filter(|track| is_vsti_output_child_track_id(&track.id))
                .count(),
            0,
            "flat output channel count alone is not multi-bus capability data"
        );
    }

    #[test]
    fn single_multichannel_bus_layout_creates_child_channels() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot = load_vsti(&mut state, &track_id, 0, "mt-power", "C:/p/mt-power.vst3");

        state.set_insert_output_bus_layout(&track_id, &slot, &[8]);
        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 8));

        let child_ids: Vec<_> = state
            .tracks
            .iter()
            .filter(|track| is_vsti_output_child_track_id(&track.id))
            .map(|track| track.id.clone())
            .collect();
        assert_eq!(
            child_ids,
            vec![
                vsti_output_child_track_id(&slot, 0),
                vsti_output_child_track_id(&slot, 1),
                vsti_output_child_track_id(&slot, 2),
                vsti_output_child_track_id(&slot, 3),
            ],
            "one mixer-only strip per flat stereo output pair"
        );
        assert_eq!(
            vsti_output_child_channels_for_bus_layout(&[8], 1),
            Some((3, 4)),
            "second child strip must read channels 3/4"
        );
    }

    /// Collapse/expand of a VSTi multi-out group is a VIEW concern: it flips the
    /// instrument insert's `multiout_collapsed` flag and changes which group keys
    /// are reported as collapsed, but it NEVER removes/recreates child mixer
    /// channels — the same child tracks (same ids) survive across collapse →
    /// expand. Audio routing is untouched (the engine snapshot ignores the flag).
    #[test]
    fn collapse_expand_multiout_group_is_view_only() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot = load_vsti(&mut state, &track_id, 0, "drums", "C:/p/drums.vst3");
        // 3 stereo output buses → 3 child mixer strips.
        state.set_insert_output_bus_layout(&track_id, &slot, &[2, 2, 2]);
        assert!(state.auto_enable_detected_insert_outputs(&track_id, &slot, 6));

        let child_ids: Vec<String> = state
            .tracks
            .iter()
            .filter(|t| is_vsti_output_child_track_id(&t.id))
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(child_ids.len(), 3);

        // Default = expanded → nothing collapsed.
        assert!(state.collapsed_vsti_output_group_keys().is_empty());

        // Collapse: flag flips, child channels remain in the model untouched.
        assert!(state.toggle_insert_multiout_collapsed(&track_id, &slot));
        assert!(state
            .collapsed_vsti_output_group_keys()
            .contains(&format!("{track_id}:{slot}")));
        let still_there: Vec<String> = state
            .tracks
            .iter()
            .filter(|t| is_vsti_output_child_track_id(&t.id))
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(
            still_there, child_ids,
            "collapse must not delete or rename child mixer channels"
        );

        // Expand: same child ids restored, flag cleared.
        assert!(!state.toggle_insert_multiout_collapsed(&track_id, &slot));
        assert!(state.collapsed_vsti_output_group_keys().is_empty());
        let after: Vec<String> = state
            .tracks
            .iter()
            .filter(|t| is_vsti_output_child_track_id(&t.id))
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(
            after, child_ids,
            "expand must reuse the same child channels"
        );
    }

    /// Test 3: load the SAME plugin file, remove, load it again → two distinct
    /// instance ids (same file is loadable as independent instances).
    #[test]
    fn same_plugin_file_reloaded_gets_distinct_instance_ids() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot_1 = load_vsti(&mut state, &track_id, 0, "synth", "C:/p/synth.vst3");
        state.remove_insert(&track_id, &slot_1);
        let slot_2 = load_vsti(&mut state, &track_id, 0, "synth", "C:/p/synth.vst3");
        assert_ne!(
            slot_1, slot_2,
            "same plugin file must reload as a new independent instance"
        );
    }

    /// Replace flow: replacing a VSTi in place yields a fresh id at the same
    /// index and clears the stale instrument pointer.
    #[test]
    fn replace_with_fresh_slot_swaps_id_in_place() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot_a = load_vsti(&mut state, &track_id, 0, "synth-a", "C:/p/a.vst3");
        let slot_b = state
            .replace_insert_with_fresh_slot(&track_id, &slot_a)
            .expect("fresh slot");
        assert_ne!(slot_a, slot_b);
        let track = state.find_track(&track_id).unwrap();
        assert_eq!(track.inserts.len(), 1, "still one slot at the same index");
        assert_eq!(track.inserts[0].id, slot_b);
        assert!(track.inserts[0].is_empty(), "fresh slot starts empty");
        assert!(
            track.instrument_plugin_instance_id.is_none(),
            "stale instrument pointer cleared until the new plugin binds"
        );
    }

    /// Removing a VSTi must also drop automation lanes bound to that instance,
    /// and leave lanes targeting other inserts / built-ins untouched.
    #[test]
    fn remove_vsti_prunes_its_automation_bindings() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot = load_vsti(&mut state, &track_id, 0, "synth", "C:/p/synth.vst3");
        {
            let track = state.tracks.iter_mut().find(|t| t.id == track_id).unwrap();
            track.automation_lanes.push(AutomationLaneState::new(
                "auto-cutoff",
                AutomationTarget::PluginParameter {
                    insert_id: slot.clone(),
                    parameter_id: "1".to_string(),
                    parameter_name: "Cutoff".to_string(),
                },
            ));
            track.automation_lanes.push(AutomationLaneState::new(
                "auto-vol",
                AutomationTarget::TrackVolume,
            ));
        }

        state.remove_insert(&track_id, &slot);

        let track = state.find_track(&track_id).unwrap();
        assert!(
            !track.automation_lanes.iter().any(|l| matches!(
                &l.target,
                AutomationTarget::PluginParameter { insert_id, .. } if *insert_id == slot
            )),
            "plugin-param automation lane must be pruned with its instance"
        );
        assert!(
            track
                .automation_lanes
                .iter()
                .any(|l| matches!(l.target, AutomationTarget::TrackVolume)),
            "unrelated track automation must survive"
        );
    }

    /// Removing an effect insert must NOT disturb the instrument pointer.
    #[test]
    fn removing_effect_keeps_instrument_pointer() {
        let mut state = TimelineState::default();
        let track_id = instrument_track(&mut state);
        let slot_instr = load_vsti(&mut state, &track_id, 0, "synth", "C:/p/synth.vst3");
        let slot_fx = load_vsti(&mut state, &track_id, 1, "fx", "C:/p/fx.vst3");
        state.remove_insert(&track_id, &slot_fx);
        let track = state.find_track(&track_id).unwrap();
        assert_eq!(
            track.instrument_plugin_instance_id.as_deref(),
            Some(slot_instr.as_str()),
            "removing an effect must not clear the instrument pointer"
        );
        assert_eq!(track.inserts.len(), 1);
    }
}

#[cfg(test)]
mod grid_lod_tests {
    use super::*;

    fn params(ppb: f32, num: u16, den: u16) -> TimelineGridLodParams {
        TimelineGridLodParams {
            pixels_per_beat: ppb,
            bpm: 120.0,
            numerator: num,
            denominator: den,
            viewport_width: 1200.0,
            scroll_x: 0.0,
        }
    }

    /// pixels_per_second that yields the requested pixels-per-beat at 120 BPM.
    fn pps_for_ppb(ppb: f32) -> f32 {
        // ppb = pps * seconds_per_beat = pps * (60/120) = pps * 0.5
        ppb / 0.5
    }

    fn zoomed_state(ppb: f32) -> TimelineState {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.viewport.pixels_per_second = pps_for_ppb(ppb);
        state.sync_pixels_per_beat();
        state.update_viewport_size(1200.0, 500.0);
        state
    }

    #[test]
    fn zoomed_in_shows_beats_and_subdivisions() {
        let lod = resolve_timeline_grid_lod(&params(120.0, 4, 4));
        assert_eq!(lod.major_bar_step, 1);
        assert!(lod.show_beat_lines);
        assert!(lod.show_subdivision_lines);
        assert_eq!(lod.subdivision_per_beat, 4); // 1/16
        assert_eq!(lod.label_bar_step, 1);
        assert!(lod.show_beat_labels);
    }

    #[test]
    fn medium_zoom_shows_bars_and_beats_without_subdivisions() {
        // 24 px/beat -> px_per_bar 96: beats visible, no subdivisions, no beat labels.
        let lod = resolve_timeline_grid_lod(&params(24.0, 4, 4));
        assert_eq!(lod.major_bar_step, 1);
        assert!(lod.show_beat_lines);
        assert!(!lod.show_subdivision_lines);
        assert!(!lod.show_beat_labels);
    }

    #[test]
    fn zoomed_out_hides_beats_and_thins_bars() {
        // 8 px/beat -> px_per_bar 32 -> every 4 bars, no beats/subs.
        let lod = resolve_timeline_grid_lod(&params(8.0, 4, 4));
        assert!(!lod.show_beat_lines);
        assert!(!lod.show_subdivision_lines);
        assert_eq!(lod.major_bar_step, 4);
        // Labels land on drawn bar lines and stay a multiple of the bar step.
        assert!(lod.label_bar_step >= lod.major_bar_step);
        assert_eq!(lod.label_bar_step % lod.major_bar_step, 0);
    }

    #[test]
    fn extreme_zoom_out_keeps_bar_lines_readable() {
        // 1 px/beat -> px_per_bar 4: bar lines must not pack tighter than ~24px.
        let lod = resolve_timeline_grid_lod(&params(1.0, 4, 4));
        assert!(!lod.show_beat_lines);
        let px_per_bar = 1.0 * beats_per_bar_from_sig(4, 4) as f32; // 4 px
        assert!(lod.major_bar_step as f32 * px_per_bar >= 24.0);
        // Labels must be at least the minimum spacing apart too.
        assert!(lod.label_bar_step as f32 * px_per_bar >= lod.min_label_px);
    }

    #[test]
    fn grid_lines_zoomed_out_emit_only_spaced_bar_lines() {
        let state = zoomed_state(8.0);
        let lines = state.get_arrangement_grid_lines(1200.0);
        assert!(!lines.is_empty());
        // No beat or subdivision lines when zoomed out.
        assert!(lines.iter().all(|l| matches!(l.level, GridLineLevel::Bar)));
        // Every drawn line is at least the minimum spacing from its neighbor.
        let mut xs: Vec<f32> = lines.iter().map(|l| l.x).collect();
        xs.sort_by(|a, b| a.total_cmp(b));
        for w in xs.windows(2) {
            assert!(w[1] - w[0] >= 3.0, "lines too close: {} vs {}", w[0], w[1]);
        }
    }

    #[test]
    fn grid_lines_zoomed_in_emit_beats_and_subs() {
        let state = zoomed_state(120.0);
        let lines = state.get_arrangement_grid_lines(1200.0);
        assert!(lines.iter().any(|l| matches!(l.level, GridLineLevel::Beat)));
        assert!(lines.iter().any(|l| matches!(l.level, GridLineLevel::Sub)));
    }

    #[test]
    fn ruler_labels_never_pack_closer_than_min_spacing() {
        for ppb in [1.0_f32, 4.0, 8.0, 16.0, 24.0, 48.0, 120.0, 300.0] {
            let state = zoomed_state(ppb);
            let lines = state.get_arrangement_grid_lines(1200.0);
            let mut label_xs: Vec<f32> =
                lines.iter().filter(|l| l.show_label).map(|l| l.x).collect();
            label_xs.sort_by(|a, b| a.total_cmp(b));
            for w in label_xs.windows(2) {
                assert!(
                    w[1] - w[0] >= 48.0 - 0.5,
                    "labels too close at ppb={ppb}: {} vs {}",
                    w[0],
                    w[1]
                );
            }
        }
    }
}

#[cfg(test)]
mod tempo_map_tests {
    use super::*;

    #[test]
    fn empty_map_uses_base_bpm() {
        let map = TempoMap::new();
        assert!(!map.has_automation());
        assert_eq!(map.bpm_at_beat(0.0, 120.0), 120.0);
        assert_eq!(map.bpm_at_beat(100.0, 120.0), 120.0);
    }

    #[test]
    fn hold_marker_steps_bpm() {
        let mut map = TempoMap::new();
        map.add_or_update_point(8.0, 140.0, TempoCurve::Hold);
        assert!(map.has_automation());
        // Before the marker we sit on the implicit base point.
        assert_eq!(map.bpm_at_beat(0.0, 120.0), 120.0);
        assert_eq!(map.bpm_at_beat(7.9, 120.0), 120.0);
        // From the marker onward the held tempo applies.
        assert_eq!(map.bpm_at_beat(8.0, 120.0), 140.0);
        assert_eq!(map.bpm_at_beat(99.0, 120.0), 140.0);
    }

    #[test]
    fn linear_curve_interpolates_between_markers() {
        let mut map = TempoMap::new();
        map.add_or_update_point(0.0, 100.0, TempoCurve::Linear);
        map.add_or_update_point(4.0, 200.0, TempoCurve::Hold);
        // Halfway between the two markers = midpoint BPM.
        assert!((map.bpm_at_beat(2.0, 120.0) - 150.0).abs() < 1e-6);
        // At/after the last marker the held value applies.
        assert!((map.bpm_at_beat(4.0, 120.0) - 200.0).abs() < 1e-6);
    }

    #[test]
    fn add_replaces_marker_at_same_beat_and_clear_resets() {
        let mut map = TempoMap::new();
        map.add_or_update_point(4.0, 130.0, TempoCurve::Hold);
        map.add_or_update_point(4.0, 150.0, TempoCurve::Linear);
        assert_eq!(map.points.len(), 1);
        assert_eq!(map.points[0].bpm, 150.0);
        assert_eq!(map.points[0].curve, TempoCurve::Linear);

        map.clear();
        assert!(!map.has_automation());
    }

    #[test]
    fn hold_tempo_time_conversions_match_engine() {
        let mut map = TempoMap::new();
        map.add_or_update_point(4.0, 160.0, TempoCurve::Hold);
        assert!((map.seconds_at_beat(0.0, 120.0) - 0.0).abs() < 1e-9);
        assert!((map.seconds_at_beat(4.0, 120.0) - 2.0).abs() < 1e-9);
        assert!((map.seconds_at_beat(8.0, 120.0) - 3.5).abs() < 1e-9);
        assert!((map.beat_at_seconds(2.0, 120.0) - 4.0).abs() < 1e-9);
        assert!((map.beat_at_seconds(3.5, 120.0) - 8.0).abs() < 1e-9);
        assert_eq!(map.samples_at_beat(4.0, 120.0, 48_000.0), 96_000);
        assert_eq!(map.samples_at_beat(8.0, 120.0, 48_000.0), 168_000);
    }

    #[test]
    fn tempo_marker_bpm_values_are_independent() {
        let mut map = TempoMap::new();
        map.add_or_update_point(0.0, 120.0, TempoCurve::Hold);
        map.add_or_update_point(4.0, 132.0, TempoCurve::Hold);
        map.ensure_point_ids();

        assert_eq!(map.points[0].bpm, 120.0);
        assert_eq!(map.points[1].bpm, 132.0);
        assert_eq!(TempoMap::format_marker_label(map.points[0].bpm), "120");
        assert_eq!(TempoMap::format_marker_label(map.points[1].bpm), "132");
        assert_eq!(map.bpm_at_beat(0.0, 120.0), 120.0);
        assert_eq!(map.bpm_at_beat(3.9, 120.0), 120.0);
        assert_eq!(map.bpm_at_beat(4.0, 120.0), 132.0);

        let id_b = map.points[1].id.clone();
        assert!(map.update_point_bpm_by_id(&id_b, 140.0));

        assert_eq!(map.points[0].bpm, 120.0);
        assert_eq!(map.points[1].bpm, 140.0);
        assert_eq!(TempoMap::format_marker_label(map.points[0].bpm), "120");
        assert_eq!(TempoMap::format_marker_label(map.points[1].bpm), "140");
        assert_eq!(map.bpm_at_beat(0.0, 120.0), 120.0);
        assert_eq!(map.bpm_at_beat(4.0, 120.0), 140.0);
    }
}

#[cfg(test)]
mod audio_asset_key_tests {
    use super::*;

    fn audio_clip(file_id: &str, source: Option<&str>) -> ClipState {
        ClipState {
            id: "c1".to_string(),
            name: "loop".to_string(),
            start_beat: 0.0,
            duration_beats: 4.0,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: file_id.to_string(),
                source_path: source.map(str::to_string),
            },
            muted: false,
            audio_import: AudioImportState::Pending,
            stretch: AudioClipStretchState::default(),
        }
    }

    #[test]
    fn asset_key_is_file_id_and_requires_a_real_source() {
        assert_eq!(
            audio_clip("asset-1", Some("C:/a/loop.wav")).audio_asset_key(),
            Some("asset-1")
        );
        // Placeholder / live-preview clip (no source) → no key.
        assert_eq!(audio_clip("asset-1", None).audio_asset_key(), None);
        // Empty asset id → no key.
        assert_eq!(
            audio_clip("", Some("C:/a/loop.wav")).audio_asset_key(),
            None
        );
    }

    #[test]
    fn binding_survives_source_path_rewrite() {
        // The whole point of keying on the asset id: a clip's waveform/import
        // binding must not break when its `source_path` is later rewritten
        // (e.g. copying the source into the project folder).
        let mut state = TimelineState::default();
        let clip_id = state.import_audio_at(
            "C:/ext/loop.wav".to_string(),
            "loop".to_string(),
            0.0,
            1.0e9,
        );

        let asset_key = state
            .find_clip(&clip_id)
            .and_then(|(_, clip)| clip.audio_asset_key())
            .expect("new audio clip has an asset key")
            .to_string();
        assert_eq!(asset_key, "C:/ext/loop.wav");

        // Simulate copy-into-project: rewrite source_path, keep file_id stable.
        for track in &mut state.tracks {
            for clip in &mut track.clips {
                if clip.id == clip_id {
                    if let ClipType::Audio { source_path, .. } = &mut clip.clip_type {
                        *source_path = Some("C:/proj/Assets/Audio/loop.wav".to_string());
                    }
                }
            }
        }

        // Asset key is unchanged despite the new path…
        assert_eq!(
            state
                .find_clip(&clip_id)
                .and_then(|(_, clip)| clip.audio_asset_key()),
            Some(asset_key.as_str())
        );
        // …and asset-keyed state updates still reach the clip.
        state.set_audio_import_for_asset(&asset_key, AudioImportState::Ready);
        assert_eq!(
            state
                .find_clip(&clip_id)
                .map(|(_, clip)| &clip.audio_import),
            Some(&AudioImportState::Ready)
        );
    }

    #[test]
    fn stretch_ratio_change_keeps_waveform_cache_key_stable() {
        let mut clip = audio_clip("asset-1", Some("C:/a/loop.wav"));
        let before = clip.audio_asset_key().map(str::to_string);
        clip.stretch.mode = StretchMode::Manual;
        clip.stretch.set_stretch_ratio(2.0);
        assert_eq!(clip.audio_asset_key(), before.as_deref());
    }

    #[test]
    fn normal_audio_resize_does_not_snap_to_grid() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.snap_to_grid = true;
        state.grid_division = SnapDivision::Div1_4;

        let clip_id =
            state.import_audio_at("C:/a/loop.wav".to_string(), "loop".to_string(), 0.0, 1.0e9);
        state.update_audio_clip_metadata("C:/a/loop.wav", "wav", 48_000, 2, 48_000, 1.0);

        assert!(state.resize_clip(&clip_id, ClipEdge::Right, 1.3));
        let clip = state.find_clip(&clip_id).map(|(_, clip)| clip).unwrap();
        assert!(
            (clip.duration_beats - 1.3).abs() < 0.001,
            "audio trim must follow cursor, not snap to grid: {}",
            clip.duration_beats
        );
        assert_eq!(clip.stretch.mode, StretchMode::Off);
        assert!(clip.stretch.source_end_samples < 48_000);
    }

    // ── Audio trim ⇒ no waveform stretch (spec §3/§4/§5/§15) ────────────────
    //
    // Invariant proving the waveform is not stretched: for an Off-mode clip the
    // active source window's real duration (source_len / source_rate) must equal
    // the clip's timeline duration (duration_beats * seconds_per_beat). When the
    // two spans stay equal, the renderer's `[0..clip_width] ↔ [source_start..
    // source_end]` mapping keeps a constant source-samples-per-pixel scale — i.e.
    // the source strip crops/reveals behind the clip window instead of being
    // squeezed to fit it.
    fn audio_time_ratio(state: &TimelineState, clip_id: &str) -> f64 {
        let (_, clip) = state.find_clip(clip_id).expect("clip");
        let source_rate = clip
            .stretch
            .original_sample_rate
            .max(clip.stretch.project_sample_rate)
            .max(1) as f64;
        let source_seconds = clip.stretch.source_len_samples() as f64 / source_rate;
        let timeline_seconds = clip.duration_beats as f64 * state.seconds_per_beat() as f64;
        source_seconds / timeline_seconds.max(f64::MIN_POSITIVE)
    }

    fn imported_audio_clip() -> (TimelineState, String) {
        let mut state = TimelineState::default();
        state.bpm = 120.0; // seconds_per_beat = 0.5
        let clip_id =
            state.import_audio_at("C:/a/loop.wav".to_string(), "loop".to_string(), 0.0, 1.0e9);
        // 48_000 frames @ 48 kHz = 1.0 s = 2.0 beats at 120 BPM.
        state.update_audio_clip_metadata("C:/a/loop.wav", "wav", 48_000, 2, 48_000, 1.0);
        (state, clip_id)
    }

    #[test]
    fn inspector_length_change_trims_audio_source_without_stretch() {
        let (mut state, clip_id) = imported_audio_clip();
        // Baseline: full clip, source span == timeline span (ratio 1.0).
        assert!((audio_time_ratio(&state, &clip_id) - 1.0).abs() < 1e-6);

        // The inspector "Length" field halves the clip. This must trim the source
        // window (right-edge semantics), NOT stretch the fixed source into the new
        // width. The audio→timeline ratio must stay 1.0.
        assert!(state.set_clip_length_trimming(&clip_id, 1.0));
        let (_, clip) = state.find_clip(&clip_id).expect("clip");
        assert_eq!(clip.stretch.mode, StretchMode::Off);
        assert_eq!(clip.stretch.source_start_samples, 0);
        assert_eq!(clip.stretch.source_end_samples, 24_000);
        assert!((clip.duration_beats - 1.0).abs() < 1e-4);
        // Playback rate unchanged, and no visual stretch.
        assert!((clip.stretch.effective_time_ratio(state.bpm as f64) - 1.0).abs() < 1e-9);
        assert!((audio_time_ratio(&state, &clip_id) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn right_edge_trim_preserves_source_scale_and_rate() {
        let (mut state, clip_id) = imported_audio_clip();
        assert!(state.resize_clip(&clip_id, ClipEdge::Right, 0.5));
        let (_, clip) = state.find_clip(&clip_id).expect("clip");
        assert_eq!(clip.stretch.source_start_samples, 0);
        assert_eq!(clip.stretch.source_end_samples, 12_000); // 0.5 beat * 0.5 s * 48k
        assert!((clip.stretch.stretch_ratio - 1.0).abs() < 1e-9); // trim never restretches
        assert!((audio_time_ratio(&state, &clip_id) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn left_edge_trim_advances_source_offset_without_stretch() {
        let (mut state, clip_id) = imported_audio_clip();
        assert!(state.resize_clip(&clip_id, ClipEdge::Left, 1.0));
        let (_, clip) = state.find_clip(&clip_id).expect("clip");
        assert!((clip.start_beat - 1.0).abs() < 1e-4);
        assert!((clip.duration_beats - 1.0).abs() < 1e-4);
        assert_eq!(clip.stretch.source_start_samples, 24_000); // 1.0 beat in
        assert_eq!(clip.stretch.source_end_samples, 48_000); // right edge fixed
        assert!((audio_time_ratio(&state, &clip_id) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn left_edge_reveal_clamps_to_media_start_without_stretch() {
        let (mut state, clip_id) = imported_audio_clip();
        // Trim in from the left, then relocate the clip so its source offset no
        // longer aligns with the timeline origin.
        assert!(state.resize_clip(&clip_id, ClipEdge::Left, 1.0));
        assert!(state.set_clip_start(&clip_id, 5.0));
        let source_start_before = state
            .find_clip(&clip_id)
            .map(|(_, c)| c.stretch.source_start_samples)
            .unwrap();
        assert_eq!(source_start_before, 24_000);

        // Ask to reveal further left than the available media (only 0.5 s / 1 beat
        // of source exists before the current offset). The timeline start must
        // clamp so the source bottoms out at 0 without stretching the waveform.
        assert!(state.resize_clip(&clip_id, ClipEdge::Left, 3.0));
        let (_, clip) = state.find_clip(&clip_id).expect("clip");
        assert_eq!(clip.stretch.source_start_samples, 0); // clamped at media start
        assert!(
            (clip.start_beat - 4.0).abs() < 1e-4,
            "start clamped to available media: {}",
            clip.start_beat
        );
        assert!((audio_time_ratio(&state, &clip_id) - 1.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod tempo_track_tests {
    use super::*;

    #[test]
    fn tempo_lane_header_subtitle_fixed_and_range() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        assert_eq!(state.tempo_lane_header_subtitle(), "Fixed 120 BPM");
        state
            .tempo_map
            .add_or_update_point(0.0, 120.0, TempoCurve::Hold);
        state
            .tempo_map
            .add_or_update_point(16.0, 160.0, TempoCurve::Hold);
        assert_eq!(state.tempo_lane_header_subtitle(), "120–160 BPM");
    }

    #[test]
    fn time_signature_lane_header_subtitle_fixed_and_markers() {
        let mut state = TimelineState::default();
        assert_eq!(state.time_signature_lane_header_subtitle(), "Fixed 4/4");
        state.time_signature_map.add_or_update_point(0.0, 4, 4);
        state.time_signature_map.add_or_update_point(16.0, 6, 8);
        assert_eq!(
            state.time_signature_lane_header_subtitle(),
            "4/4 · 2 markers"
        );
    }

    /// Structure, tempo and meter lanes are on by default; Song Text is opt-in.
    #[test]
    fn default_global_lanes_are_structure_tempo_and_time_signature() {
        let state = TimelineState::default();
        assert!(state.show_region_track);
        assert!(state.show_marker_track);
        assert!(state.show_tempo_track);
        assert!(state.show_time_signature_track);
        assert!(!state.show_song_text_track);
        assert_eq!(
            state.visible_global_lanes(),
            vec![
                GlobalLaneKind::Arranger,
                GlobalLaneKind::Marker,
                GlobalLaneKind::Tempo,
                GlobalLaneKind::TimeSignature
            ]
        );
    }

    /// Turning the conductor lanes on by default must not make the transport
    /// claim the project has tempo automation or meter changes.
    ///
    /// The tempo map stays empty — the lane renders the effective BPM as an
    /// implicit flag rather than writing an anchor point, because
    /// `tempo_has_automation()` is `!points.is_empty()` and feeds the engine's
    /// tempo path in several places. The time-signature map does carry a
    /// default 4/4 at beat 0, which is the project's meter rather than a
    /// change, and `time_signature_has_markers()` correctly ignores it.
    #[test]
    fn default_lanes_do_not_fabricate_automation() {
        let state = TimelineState::default();
        assert!(state.tempo_map.points.is_empty());
        assert!(!state.tempo_has_automation());
        assert!(!state.time_signature_has_markers());
    }

    #[test]
    fn show_tempo_track_enables_global_lane() {
        let mut state = TimelineState::default();
        // Isolate the tempo lane from the structure lanes so this stays a test
        // about one toggle rather than about the default lane set.
        state.hide_region_track_lane();
        state.hide_marker_track_lane();

        state.hide_tempo_track_lane();
        assert!(!state.show_tempo_track);
        assert_eq!(
            state.visible_global_lanes(),
            vec![GlobalLaneKind::TimeSignature]
        );

        state.show_tempo_track_lane();
        assert!(state.show_tempo_track);
        assert_eq!(
            state.visible_global_lanes(),
            vec![GlobalLaneKind::Tempo, GlobalLaneKind::TimeSignature]
        );
    }

    /// A lane counted in `global_lanes_height` but not drawn (or the reverse)
    /// offsets every window-y -> arrangement-y conversion.
    #[test]
    fn song_text_lane_height_follows_visibility() {
        let mut state = TimelineState::default();
        assert_eq!(state.song_text_track_height(), 0.0);
        let hidden = state.global_lanes_height();

        state.show_song_text_track_lane();
        assert!(state.song_text_track_height() > 0.0);
        assert_eq!(
            state.global_lanes_height(),
            hidden + state.song_text_track_height()
        );

        state.hide_song_text_track_lane();
        assert_eq!(state.global_lanes_height(), hidden);
    }

    #[test]
    fn tempo_track_renders_two_point_bpm_values() {
        let mut state = TimelineState::default();
        state
            .tempo_map
            .add_or_update_point(0.0, 120.0, TempoCurve::Hold);
        state
            .tempo_map
            .add_or_update_point(4.0, 160.0, TempoCurve::Hold);
        state.show_tempo_track_lane();

        let values = state.tempo_track_render_bpm_values();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], 120.0);
        assert_eq!(values[1], 160.0);
        assert_eq!(TempoMap::format_marker_label(values[0]), "120");
        assert_eq!(TempoMap::format_marker_label(values[1]), "160");
    }

    #[test]
    fn editing_one_tempo_point_leaves_other_unchanged() {
        let mut state = TimelineState::default();
        state
            .tempo_map
            .add_or_update_point(0.0, 120.0, TempoCurve::Hold);
        state
            .tempo_map
            .add_or_update_point(4.0, 160.0, TempoCurve::Hold);
        state.tempo_map.ensure_point_ids();
        let id_b = state.tempo_map.points[1].id.clone();
        let rev_before = state.tempo_map.revision();

        assert!(state.move_tempo_point(&id_b, 4.0, 170.0));
        assert_eq!(state.tempo_map.points[0].bpm, 120.0);
        assert_eq!(state.tempo_map.points[1].bpm, 170.0);
        assert!(state.tempo_map.revision() > rev_before);
    }

    #[test]
    fn fixed_tempo_renders_flat_line_across_viewport() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state
            .tempo_map
            .reset_to_single_point(0.0, 120.0, TempoCurve::Hold);
        state.show_tempo_track_lane();
        state.update_viewport_size(800.0, 500.0);

        let samples = state.tempo_track_bpm_samples(800.0);
        assert!(!samples.is_empty());
        for bpm in samples {
            assert!((bpm - 120.0).abs() < 1e-6);
        }
    }
}

#[cfg(test)]
mod time_signature_map_tests {
    use super::*;

    #[test]
    fn default_4_4_bar_boundaries() {
        let map = TimeSignatureMap::with_default_4_4();
        assert!((map.bar_start_beat(1) - 0.0).abs() < 1e-9);
        assert!((map.bar_start_beat(2) - 4.0).abs() < 1e-9);
        assert!((map.bar_start_beat(3) - 8.0).abs() < 1e-9);
        let bb0 = map.bar_beat_at_beat(0.0);
        assert_eq!(bb0.bar, 1);
        assert_eq!(bb0.beat_in_bar, 1);
        let bb4 = map.bar_beat_at_beat(4.0);
        assert_eq!(bb4.bar, 2);
        assert_eq!(bb4.beat_in_bar, 1);
    }

    #[test]
    fn change_from_4_4_to_3_4() {
        let mut map = TimeSignatureMap::with_default_4_4();
        map.add_or_update_point(16.0, 3, 4);
        assert_eq!(map.format_position_at_beat(0.0), "1.1");
        assert_eq!(map.format_position_at_beat(4.0), "2.1");
        assert_eq!(map.format_position_at_beat(8.0), "3.1");
        assert_eq!(map.format_position_at_beat(12.0), "4.1");
        assert_eq!(map.format_position_at_beat(16.0), "5.1");
        assert_eq!(map.format_position_at_beat(19.0), "6.1");
        assert_eq!(map.format_position_at_beat(22.0), "7.1");
    }

    #[test]
    fn seven_eight_beats_per_bar() {
        let mut map = TimeSignatureMap::new();
        map.add_or_update_point(0.0, 7, 8);
        assert!((map.beats_per_bar_at_beat(0.0) - 3.5).abs() < 1e-9);
        assert!((map.bar_start_beat(1) - 0.0).abs() < 1e-9);
        assert!((map.bar_start_beat(2) - 3.5).abs() < 1e-9);
        assert!((map.bar_start_beat(3) - 7.0).abs() < 1e-9);
    }

    #[test]
    fn marker_bpm_values_are_independent() {
        let mut map = TimeSignatureMap::with_default_4_4();
        map.add_or_update_point(16.0, 3, 4);
        map.ensure_point_ids();
        assert_eq!(map.points[0].label(), "4/4");
        assert_eq!(map.points[1].label(), "3/4");
        let id_b = map.points[1].id.clone();
        assert!(map.update_point_by_id(&id_b, 7, 8));
        assert_eq!(map.points[0].label(), "4/4");
        assert_eq!(map.points[1].label(), "7/8");
    }

    #[test]
    fn five_eight_ruler_denominator_ticks() {
        let mut map = TimeSignatureMap::new();
        map.add_or_update_point(0.0, 5, 8);
        assert!((map.bar_start_beat(1) - 0.0).abs() < 1e-9);
        assert!((map.bar_start_beat(2) - 2.5).abs() < 1e-9);
        assert_eq!(map.format_position_at_beat(0.0), "1.1");
        assert_eq!(map.format_position_at_beat(0.5), "1.2");
        assert_eq!(map.format_position_at_beat(1.0), "1.3");
        assert_eq!(map.format_position_at_beat(1.5), "1.4");
        assert_eq!(map.format_position_at_beat(2.0), "1.5");
        assert_eq!(map.format_position_at_beat(2.5), "2.1");
    }

    #[test]
    fn six_eight_ruler_denominator_ticks() {
        let mut map = TimeSignatureMap::new();
        map.add_or_update_point(0.0, 6, 8);
        assert!((map.bar_start_beat(2) - 3.0).abs() < 1e-9);
        assert_eq!(map.format_position_at_beat(2.5), "1.6");
        assert_eq!(map.format_position_at_beat(3.0), "2.1");
    }

    #[test]
    fn default_grouping_for_compound_meters() {
        let pt = TimeSignaturePoint::new(0.0, 5, 8);
        assert_eq!(pt.effective_grouping(), vec![2, 3]);
        let pt6 = TimeSignaturePoint::new(0.0, 6, 8);
        assert_eq!(pt6.effective_grouping(), vec![3, 3]);
        let pt7 = TimeSignaturePoint::new(0.0, 7, 8);
        assert_eq!(pt7.effective_grouping(), vec![2, 2, 3]);
    }

    #[test]
    fn marker_boundary_label_meter_change() {
        let mut map = TimeSignatureMap::new();
        map.add_or_update_point(0.0, 5, 8);
        map.add_or_update_point(2.5, 6, 8);
        assert_eq!(map.format_position_at_beat(2.0), "1.5");
        assert_eq!(map.format_position_at_beat(2.5), "2.1");
        assert_eq!(map.format_position_at_beat(3.0), "2.2");
    }

    #[test]
    fn visible_bar_background_rects_across_changing_meters() {
        let mut map = TimeSignatureMap::new();
        map.add_or_update_point(0.0, 5, 8);
        map.add_or_update_point(2.5, 6, 8);
        map.add_or_update_point(5.5, 5, 8);
        let rects = map.visible_bar_rects(0.0, 8.0);
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].bar, 1);
        assert!((rects[0].start_beat - 0.0).abs() < 1e-9);
        assert!((rects[0].end_beat - 2.5).abs() < 1e-9);
        assert_eq!(rects[1].bar, 2);
        assert!((rects[1].start_beat - 2.5).abs() < 1e-9);
        assert!((rects[1].end_beat - 5.5).abs() < 1e-9);
        assert_eq!(rects[2].bar, 3);
        assert!((rects[2].start_beat - 5.5).abs() < 1e-9);
        assert!((rects[2].end_beat - 8.0).abs() < 1e-9);
    }

    #[test]
    fn visible_bar_rects_follow_scroll_window() {
        let mut map = TimeSignatureMap::new();
        map.add_or_update_point(0.0, 5, 8);
        map.add_or_update_point(2.5, 6, 8);
        map.add_or_update_point(5.5, 5, 8);
        let rects = map.visible_bar_rects(3.0, 6.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].bar, 2);
        assert!((rects[0].start_beat - 2.5).abs() < 1e-9);
        assert_eq!(rects[1].bar, 3);
        assert!((rects[1].start_beat - 5.5).abs() < 1e-9);
    }
}

#[cfg(test)]
mod midi_edit_tests {
    use super::*;
    use crate::components::edit::{EditCommand, TrackSnapshot};

    /// Build an empty state with one MIDI clip and return `(state, clip_id)`.
    pub(super) fn state_with_midi_clip() -> (TimelineState, String) {
        let mut state = TimelineState::default();
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Midi,
            name: "Test".into(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let clip = state
            .build_midi_clip(&track_id, 0.0, 4.0)
            .expect("clip builds");
        let clip_id = clip.id.clone();
        EditCommand::CreateClip { track_id, clip }.execute(&mut state);
        (state, clip_id)
    }

    pub(super) fn note(state: &TimelineState, clip_id: &str, id: u64) -> MidiNoteState {
        state
            .midi_clip_notes(clip_id)
            .unwrap()
            .iter()
            .find(|n| n.id == id)
            .cloned()
            .unwrap()
    }

    /// Analyze Accent has to be one undoable step, and undoing it has to put
    /// every note back — including notes it never touched.
    ///
    /// Two hundred analysed notes producing two hundred undo entries would make
    /// Ctrl+Z useless after running it, and an `EditMidiNotes` that forgot to
    /// carry `accent` would apply and never come back.
    #[test]
    fn an_accent_analysis_is_one_undoable_edit_that_restores_every_note() {
        let (mut state, clip_id) = state_with_midi_clip();
        let ids: Vec<u64> = (0..8)
            .map(|index| {
                state
                    .add_midi_note(&clip_id, 60 + index as u8, index as f32 * 0.5, 0.5, 96)
                    .expect("note added")
            })
            .collect();

        // One note is already hand-edited; a re-analysis that preserves manual
        // edits must leave it alone, and undo must not resurrect a value it
        // never wrote.
        state.set_midi_notes_accent_bulk(&clip_id, &ids[3..4], 0.9);
        let before: Vec<MidiNoteState> = state.midi_clip_notes(&clip_id).unwrap().clone();
        assert!(before[3].accent.is_some());
        assert!(before[0].accent.is_none());

        // What an analysis produces: one accent per note, applied in one go.
        let analysed: Vec<AccentState> = (0..ids.len())
            .map(|index| AccentState::generated(0.2 + 0.05 * index as f32, 0.5, 0.5, 0.5, 0.7))
            .collect();
        let mut after = before.clone();
        let changed = crate::solfege::accent::apply_accents(
            &mut after,
            &analysed,
            crate::solfege::AccentReplacePolicy::KeepManual,
        );
        assert_eq!(changed, 7, "the hand-edited note was left alone");

        let command = EditCommand::EditMidiNotes {
            clip_id: clip_id.clone(),
            prev: before.clone(),
            next: after.clone(),
        };
        command.execute(&mut state);
        assert_eq!(
            note(&state, &clip_id, ids[0]).accent.unwrap().prominence,
            0.2
        );
        assert_eq!(
            note(&state, &clip_id, ids[3]).accent.unwrap().prominence,
            0.9,
            "the hand-set accent survived the analysis"
        );

        command.undo(&mut state);
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(
                note(&state, &clip_id, *id).accent,
                before[index].accent,
                "note {index} did not come back"
            );
        }
        assert!(note(&state, &clip_id, ids[0]).accent.is_none());

        command.execute(&mut state);
        assert_eq!(
            note(&state, &clip_id, ids[0]).accent.unwrap().prominence,
            0.2,
            "redo did not reapply the analysis"
        );
    }

    /// Clearing an accent returns the note to "never analysed", which is a
    /// different state from "analysed and found neutral" — the first is filled
    /// in by the next analysis and the second is not.
    #[test]
    fn clearing_an_accent_is_distinct_from_setting_it_to_neutral() {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state
            .add_midi_note(&clip_id, 60, 0.0, 1.0, 96)
            .expect("note added");

        state.set_midi_notes_accent_bulk(&clip_id, &[id], 0.5);
        let neutral = note(&state, &clip_id, id).accent.expect("an accent");
        assert!(neutral.is_neutral());
        assert_eq!(neutral.source, AccentSource::Manual);

        assert_eq!(state.clear_midi_notes_accent(&clip_id, &[id]), 1);
        assert!(note(&state, &clip_id, id).accent.is_none());
        // Clearing twice changes nothing, so it cannot record an empty edit.
        assert_eq!(state.clear_midi_notes_accent(&clip_id, &[id]), 0);
    }

    #[test]
    fn midi_resize_uses_shared_snap_and_shift_bypass() {
        let (mut state, clip_id) = state_with_midi_clip();
        state.snap_to_grid = true;
        state.grid_division = SnapDivision::Div1_4;

        assert!(state.resize_clip(&clip_id, ClipEdge::Right, 5.6));
        let (_, snapped) = state.find_clip(&clip_id).expect("clip");
        assert!((snapped.duration_beats - 6.0).abs() < 1.0e-6);

        assert!(state.resize_clip_with_bypass(&clip_id, ClipEdge::Right, 5.6, true));
        let (_, bypassed) = state.find_clip(&clip_id).expect("clip");
        assert!((bypassed.duration_beats - 5.6).abs() < 1.0e-6);
    }

    #[test]
    fn delete_track_command_undo_redo_restores_track_position() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let first_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "First".into(),
            color: gpui::Rgba {
                r: 0.1,
                g: 0.1,
                b: 0.1,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        let second_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Second".into(),
            color: gpui::Rgba {
                r: 0.2,
                g: 0.2,
                b: 0.2,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        state.selection.selected_track_id = Some(second_id.clone());

        let snapshot = TrackSnapshot::capture(&state, &second_id).expect("track snapshot");
        let cmd = EditCommand::DeleteTrack { snapshot };

        cmd.execute(&mut state);
        assert_eq!(state.tracks.len(), 1);
        assert_eq!(state.tracks[0].id, first_id);

        cmd.undo(&mut state);
        assert_eq!(state.tracks.len(), 2);
        assert_eq!(state.tracks[1].id, second_id);
        assert_eq!(
            state.selection.selected_track_id.as_deref(),
            Some(second_id.as_str())
        );

        cmd.execute(&mut state);
        assert_eq!(state.tracks.len(), 1);
        assert_eq!(state.tracks[0].id, first_id);
    }

    #[test]
    fn edit_midi_notes_velocity_roundtrips() {
        let (mut state, clip_id) = state_with_midi_clip();
        let first = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 40).unwrap();
        let second = state.add_midi_note(&clip_id, 64, 1.0, 1.0, 80).unwrap();

        let prev = state.midi_clip_notes(&clip_id).unwrap().clone();
        state.set_midi_note_velocity(&clip_id, first, 55);
        state.set_midi_note_velocity(&clip_id, second, 95);
        let next = state.midi_clip_notes(&clip_id).unwrap().clone();
        assert_eq!(note(&state, &clip_id, first).velocity, 55);
        assert_eq!(note(&state, &clip_id, second).velocity, 95);

        let cmd = EditCommand::EditMidiNotes {
            clip_id: clip_id.clone(),
            prev,
            next,
        };
        cmd.undo(&mut state);
        assert_eq!(
            note(&state, &clip_id, first).velocity,
            40,
            "undo restores first"
        );
        assert_eq!(
            note(&state, &clip_id, second).velocity,
            80,
            "undo restores second"
        );
        cmd.execute(&mut state);
        assert_eq!(
            note(&state, &clip_id, first).velocity,
            55,
            "redo reapplies first"
        );
        assert_eq!(
            note(&state, &clip_id, second).velocity,
            95,
            "redo reapplies second"
        );
    }

    #[test]
    fn edit_midi_notes_move_roundtrips() {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();

        let prev = state.midi_clip_notes(&clip_id).unwrap().clone();
        state.move_midi_notes(&clip_id, &[(id, 2.0, 67)]);
        let next = state.midi_clip_notes(&clip_id).unwrap().clone();

        let cmd = EditCommand::EditMidiNotes {
            clip_id: clip_id.clone(),
            prev,
            next,
        };
        cmd.undo(&mut state);
        let n = note(&state, &clip_id, id);
        assert_eq!((n.start, n.pitch), (0.0, 60), "undo restores start+pitch");
        cmd.execute(&mut state);
        let n = note(&state, &clip_id, id);
        assert_eq!((n.start, n.pitch), (2.0, 67), "redo reapplies");
    }

    #[test]
    fn controller_point_edit_and_undo_roundtrip() {
        let (mut state, clip_id) = state_with_midi_clip();
        let kind = MidiControllerKind::CC(1);
        let prev = state.controller_points_snapshot(&clip_id, kind);
        state.put_controller_point(&clip_id, kind, 1.0, 0.5);
        state.put_controller_point(&clip_id, kind, 2.0, 0.75);
        let next = state.controller_points_snapshot(&clip_id, kind);
        assert_eq!(next.len(), 2);

        let cmd = EditCommand::SetControllerPoints {
            clip_id: clip_id.clone(),
            kind,
            prev,
            next,
        };
        cmd.undo(&mut state);
        assert_eq!(
            state.controller_points_snapshot(&clip_id, kind).len(),
            0,
            "undo clears the lane"
        );
        cmd.execute(&mut state);
        assert_eq!(
            state.controller_points_snapshot(&clip_id, kind).len(),
            2,
            "redo restores points"
        );
    }

    #[test]
    fn controller_undo_to_empty_removes_lane_and_redo_restores_pitch_bend() {
        let (mut state, clip_id) = state_with_midi_clip();
        let kind = MidiControllerKind::PitchBend;
        let prev = state.controller_points_snapshot(&clip_id, kind);
        state.put_controller_point(&clip_id, kind, 1.0, 0.0);
        let next = state.controller_points_snapshot(&clip_id, kind);
        assert_eq!(next.len(), 1);

        let cmd = EditCommand::SetControllerPoints {
            clip_id: clip_id.clone(),
            kind,
            prev,
            next,
        };
        cmd.undo(&mut state);
        assert!(
            state
                .midi_clip_controller_lanes(&clip_id)
                .is_some_and(|lanes| lanes.iter().all(|lane| lane.kind != kind)),
            "undo to an empty snapshot removes the controller lane"
        );
        cmd.execute(&mut state);
        let restored = state.controller_points_snapshot(&clip_id, kind);
        assert_eq!(restored.len(), 1, "redo restores pitch-bend points");
        assert_eq!(restored[0].value, 0.0);
    }

    #[test]
    fn put_controller_point_merges_within_epsilon() {
        let (mut state, clip_id) = state_with_midi_clip();
        let kind = MidiControllerKind::CC(7);
        state.put_controller_point(&clip_id, kind, 1.0, 0.2);
        state.put_controller_point(&clip_id, kind, 1.0, 0.9);
        let pts = state.controller_points_snapshot(&clip_id, kind);
        assert_eq!(pts.len(), 1, "same-beat edit updates in place");
        assert_eq!(pts[0].value, 0.9);
    }

    #[test]
    fn set_controller_point_moves_in_place() {
        let (mut state, clip_id) = state_with_midi_clip();
        let kind = MidiControllerKind::CC(1);
        state.put_controller_point(&clip_id, kind, 1.0, 0.5);
        let id = state.controller_points_snapshot(&clip_id, kind)[0].id;
        assert!(state.set_controller_point(&clip_id, kind, id, 3.0, 0.25));
        let snapshot = state.controller_points_snapshot(&clip_id, kind);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].beat, 3.0);
        assert_eq!(snapshot[0].value, 0.25);
        assert_eq!(snapshot[0].id, id, "id is preserved across a move");
    }

    #[test]
    fn delete_controller_points_near_removes_in_tolerance() {
        let (mut state, clip_id) = state_with_midi_clip();
        let kind = MidiControllerKind::CC(11);
        state.put_controller_point(&clip_id, kind, 1.0, 0.5);
        state.put_controller_point(&clip_id, kind, 3.0, 0.5);
        let removed = state.delete_controller_points_near(&clip_id, kind, 1.05, 0.25);
        assert_eq!(removed, 1);
        assert_eq!(state.controller_points_snapshot(&clip_id, kind).len(), 1);
    }

    #[test]
    fn set_midi_notes_muted_roundtrips() {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        assert!(!note(&state, &clip_id, id).muted);

        let cmd = EditCommand::SetMidiNotesMuted {
            clip_id: clip_id.clone(),
            prev: vec![(id, false)],
            muted: true,
        };
        cmd.execute(&mut state);
        assert!(note(&state, &clip_id, id).muted, "execute mutes");
        cmd.undo(&mut state);
        assert!(!note(&state, &clip_id, id).muted, "undo unmutes");
    }

    #[test]
    fn new_midi_note_defaults_to_channel_one() {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        assert_eq!(note(&state, &clip_id, id).channel.ui(), 1);
    }

    #[test]
    fn set_midi_notes_channel_updates_selected_only() {
        let (mut state, clip_id) = state_with_midi_clip();
        let a = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        let b = state.add_midi_note(&clip_id, 64, 1.0, 1.0, 100).unwrap();

        let changed = state.set_midi_notes_channel(&clip_id, &[a], MidiChannel::from_ui(5));
        assert_eq!(changed, 1);
        assert_eq!(note(&state, &clip_id, a).channel.ui(), 5);
        assert_eq!(note(&state, &clip_id, b).channel.ui(), 1, "b untouched");

        // No-op when already on the target channel.
        let changed = state.set_midi_notes_channel(&clip_id, &[a], MidiChannel::from_ui(5));
        assert_eq!(changed, 0);
    }

    #[test]
    fn nudge_midi_notes_channel_clamps_into_range() {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 1.0, 100).unwrap();
        state.set_midi_notes_channel(&clip_id, &[id], MidiChannel::from_ui(16));

        let changed = state.nudge_midi_notes_channel(&clip_id, &[id], 5);
        assert_eq!(changed, 0, "already clamped at 16, no-op");
        assert_eq!(note(&state, &clip_id, id).channel.ui(), 16);

        state.nudge_midi_notes_channel(&clip_id, &[id], -20);
        assert_eq!(
            note(&state, &clip_id, id).channel.ui(),
            1,
            "clamps down to 1"
        );
    }

    #[test]
    fn split_midi_note_roundtrips() {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state.add_midi_note(&clip_id, 60, 0.0, 2.0, 100).unwrap();
        let original = note(&state, &clip_id, id).clone();
        let left = MidiNoteState::new(60, 0.0, 1.0, 100);
        let right = MidiNoteState::new(60, 1.0, 1.0, 100);
        let (left_id, right_id) = (left.id, right.id);

        let cmd = EditCommand::SplitMidiNote {
            clip_id: clip_id.clone(),
            original,
            parts: vec![left, right],
        };
        cmd.execute(&mut state);
        let notes = state.midi_clip_notes(&clip_id).unwrap();
        assert!(notes.iter().all(|n| n.id != id), "original removed");
        assert!(notes.iter().any(|n| n.id == left_id), "left part added");
        assert!(notes.iter().any(|n| n.id == right_id), "right part added");

        cmd.undo(&mut state);
        let notes = state.midi_clip_notes(&clip_id).unwrap();
        assert!(notes.iter().any(|n| n.id == id), "undo restores original");
        assert!(
            notes.iter().all(|n| n.id != left_id && n.id != right_id),
            "undo removes both parts"
        );
    }

    #[test]
    fn update_region_range_normalizes_and_sorts_regions() {
        let mut state = TimelineState::default();
        let early = state.add_region_at_beat(4.0);
        let late = state.add_region_at_beat(12.0);

        assert!(state.update_region_range(&late, 2.0, 1.0));

        let moved = state
            .regions
            .iter()
            .find(|region| region.id == late)
            .expect("updated region exists");
        assert_eq!(moved.normalized_range(), (1.0, 2.0));
        assert_eq!(state.regions[0].id, late, "regions stay sorted by start");
        assert_eq!(state.regions[1].id, early);
    }
}

/// FX-chain drag reorder (Slice B): model order ops, the gap-math helper, and
/// the `ReorderFxSlot` undo command. Verifies reorder never recreates instances
/// and that per-instance state (bypass / preset / parameters) follows the id.
#[cfg(test)]
mod fx_reorder_tests {
    use super::*;
    use crate::components::edit::edit_commands::{EditCommand, EditHistory};

    /// Audio track with three effect inserts loaded; returns (track_id, [a,b,c]).
    fn track_with_three_fx(state: &mut TimelineState) -> (String, [String; 3]) {
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
            input_monitor: InputMonitorMode::Off,
        });
        let mut ids = Vec::new();
        for (i, name) in ["fx-a", "fx-b", "fx-c"].iter().enumerate() {
            let slot = state.ensure_insert_slot_at(&track_id, i).expect("slot");
            state.set_insert_plugin(
                &track_id,
                &slot,
                name.to_string(),
                Some(std::path::PathBuf::from(format!("C:/p/{name}.vst3"))),
                InsertPluginFormat::Vst3,
                None,
                name.to_string(),
            );
            ids.push(slot);
        }
        (track_id, [ids[0].clone(), ids[1].clone(), ids[2].clone()])
    }

    #[test]
    fn set_insert_order_reorders_in_place_and_reports_change() {
        let mut state = TimelineState::default();
        let (track, [a, b, c]) = track_with_three_fx(&mut state);

        // A,B,C -> B,A,C
        assert!(state.set_insert_order(&track, &[b.clone(), a.clone(), c.clone()]));
        assert_eq!(
            state.insert_order(&track),
            vec![b.clone(), a.clone(), c.clone()]
        );
        // Idempotent: re-applying the same order is a no-op (no undo churn).
        assert!(!state.set_insert_order(&track, &[b.clone(), a.clone(), c.clone()]));
    }

    #[test]
    fn reorder_preserves_per_instance_state() {
        let mut state = TimelineState::default();
        let (track, [a, b, c]) = track_with_three_fx(&mut state);

        // Bypass B and give it a captured plugin-state blob + a parameter.
        assert_eq!(state.toggle_insert_bypass(&track, &b), Some(true));
        {
            let slot = state
                .insert_slots_mut(&track)
                .unwrap()
                .iter_mut()
                .find(|s| s.id == b)
                .unwrap();
            slot.vst3_state = Some(std::sync::Arc::new(vec![1, 2, 3, 4]));
            slot.parameters.push(PluginParameterState {
                id: 7,
                name: "Cutoff".to_string(),
                value_normalized: 0.5,
                automatable: true,
                hidden: false,
                read_only: false,
                unit: String::new(),
            });
        }

        // Reorder B to the front.
        state.set_insert_order(&track, &[b.clone(), a.clone(), c.clone()]);

        let slot_b = state.find_insert_slot(&track, &b).expect("b survives");
        assert_eq!(slot_b.id, b, "instance id is unchanged by reorder");
        assert!(slot_b.bypassed, "bypass follows the instance");
        assert_eq!(
            slot_b.vst3_state.as_deref(),
            Some(&vec![1, 2, 3, 4]),
            "preset/state follows the instance"
        );
        assert_eq!(slot_b.parameters.len(), 1, "parameters follow the instance");
        // Sanity: still exactly three slots, no recreation, A and C intact.
        assert_eq!(state.insert_order(&track), vec![b, a, c]);
    }

    #[test]
    fn reordered_insert_ids_gap_math() {
        let ids = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        // Move A down into the B|C gap (gap 2) -> B,A,C.
        assert_eq!(
            TimelineState::reordered_insert_ids(&ids, "A", 2),
            vec!["B", "A", "C"]
        );
        // Move C up into the A|B gap (gap 1) -> A,C,B.
        assert_eq!(
            TimelineState::reordered_insert_ids(&ids, "C", 1),
            vec!["A", "C", "B"]
        );
        // Drop into the same place (gap 0 / gap before itself) is a no-op.
        assert_eq!(
            TimelineState::reordered_insert_ids(&ids, "A", 0),
            vec!["A", "B", "C"]
        );
        // Drop at the very end (gap == len) -> append.
        assert_eq!(
            TimelineState::reordered_insert_ids(&ids, "A", 3),
            vec!["B", "C", "A"]
        );
        // Unknown id leaves the order untouched.
        assert_eq!(
            TimelineState::reordered_insert_ids(&ids, "Z", 1),
            vec!["A", "B", "C"]
        );
    }

    #[test]
    fn reorder_fx_slot_command_undo_redo_is_exact() {
        let mut state = TimelineState::default();
        let (track, [a, b, c]) = track_with_three_fx(&mut state);
        let before = state.insert_order(&track); // [a,b,c]
        let after = vec![b.clone(), a.clone(), c.clone()]; // [b,a,c]

        let mut history = EditHistory::new(16);
        let cmd = EditCommand::ReorderFxSlot {
            track_id: track.clone(),
            before_order: before.clone(),
            after_order: after.clone(),
        };
        cmd.execute(&mut state);
        history.push(cmd);
        assert_eq!(
            state.insert_order(&track),
            after,
            "execute applies new order"
        );

        assert!(history.undo(&mut state));
        assert_eq!(state.insert_order(&track), before, "undo restores order");

        assert!(history.redo(&mut state));
        assert_eq!(state.insert_order(&track), after, "redo re-applies order");

        // Instance ids are stable across the whole cycle (no recreation).
        let mut sorted = state.insert_order(&track);
        sorted.sort();
        let mut expected = vec![a, b, c];
        expected.sort();
        assert_eq!(sorted, expected);
    }
}

#[cfg(test)]
mod midi_import_tests {
    use super::*;
    use crate::components::timeline::midi_import::{ImportedMidiClip, ImportedMidiTrack};

    fn imported_track(name: &str, pitch: u8) -> ImportedMidiTrack {
        ImportedMidiTrack {
            name: Some(name.to_string()),
            channel_hint: None,
            clip: ImportedMidiClip {
                notes: vec![MidiNoteState::new(pitch, 0.0, 1.0, 100)],
                controller_lanes: Vec::new(),
                sysex_events: Vec::new(),
                markers: Vec::new(),
                duration_beats: 4.0,
            },
        }
    }

    /// Regression test: importing a multi-track MIDI batch (e.g. a
    /// channel-split file) must give every resulting clip a distinct id.
    /// `next_clip_id()` only sees clips already attached to `state.tracks`,
    /// so before this fix, clips built earlier in the same batch (still only
    /// in the local `clips` Vec, not yet pushed to any track) were invisible
    /// to later `next_clip_id()` calls and all received the same id — which
    /// broke solo-selecting a single clip, since selection matches by id and
    /// every split clip compared equal.
    #[test]
    fn multi_track_import_assigns_distinct_clip_ids() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let imported = vec![
            imported_track("Ch 1", 60),
            imported_track("Ch 2", 64),
            imported_track("Ch 3", 67),
        ];
        let clips = state.import_midi_tracks_at("Song".to_string(), imported, 0.0, 0.0);

        assert_eq!(clips.len(), 3, "all three channel clips should build");
        let ids: std::collections::HashSet<&String> =
            clips.iter().map(|(_, clip)| &clip.id).collect();
        assert_eq!(
            ids.len(),
            3,
            "every imported clip must have a unique id, got {:?}",
            clips.iter().map(|(_, c)| &c.id).collect::<Vec<_>>()
        );
        let track_ids: std::collections::HashSet<&String> =
            clips.iter().map(|(track_id, _)| track_id).collect();
        assert_eq!(
            track_ids.len(),
            3,
            "each channel should land on its own track"
        );
    }
}

#[cfg(test)]
mod midi_output_routing_tests {
    use super::*;

    fn track(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    /// A MIDI track routed to a real Instrument track resolves to that
    /// instrument for both playback (`engine_snapshot`) and live preview.
    #[test]
    fn midi_track_routes_to_instrument_target() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let inst_id = track(&mut state, TrackType::Instrument, "Synth");
        let midi_id = track(&mut state, TrackType::Midi, "Notes");
        state.set_track_output_routing(
            &midi_id,
            TrackOutputRouting::Instrument {
                track_id: inst_id.clone(),
            },
        );

        assert_eq!(
            state.effective_instrument_track_id(&midi_id),
            Some(inst_id.clone())
        );
        assert_eq!(state.effective_instrument_track_id(&inst_id), Some(inst_id));
    }

    /// An unrouted MIDI track (default `TrackOutputRouting::None`) has no
    /// effective instrument — it should stay silent rather than guessing a
    /// target, matching the "no silent misrouting" rule.
    #[test]
    fn unrouted_midi_track_has_no_effective_instrument() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let midi_id = track(&mut state, TrackType::Midi, "Notes");
        assert_eq!(state.effective_instrument_track_id(&midi_id), None);
    }

    /// Routing to a track id that no longer exists (or isn't an Instrument
    /// track anymore) resolves to `None` instead of panicking or guessing.
    #[test]
    fn stale_instrument_target_resolves_to_none() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let midi_id = track(&mut state, TrackType::Midi, "Notes");
        state.set_track_output_routing(
            &midi_id,
            TrackOutputRouting::Instrument {
                track_id: "does-not-exist".to_string(),
            },
        );
        assert_eq!(state.effective_instrument_track_id(&midi_id), None);
    }
}

#[cfg(test)]
mod audio_clip_split_tests {
    use super::*;

    fn audio_clip(id: &str, start_beat: f32, duration_beats: f32, offset_beats: f32) -> ClipState {
        ClipState {
            id: id.to_string(),
            name: "Take".to_string(),
            start_beat,
            duration_beats,
            source_duration_seconds: Some(30.0),
            offset_beats,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "asset-1".to_string(),
                source_path: Some("/proj/Assets/take.wav".to_string()),
            },
            muted: false,
            audio_import: AudioImportState::Pending,
            stretch: AudioClipStretchState::default(),
        }
    }

    #[test]
    fn split_divides_length_and_carries_offset() {
        let state = TimelineState::default();
        let mut clip = audio_clip("clip-5", 4.0, 8.0, 2.0);
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = 192_000;
        clip.stretch.source_end_samples = 192_000;
        let (left, right) = state
            .plan_audio_clip_split(&clip, 8.0)
            .expect("split inside the clip should produce two clips");

        // Left keeps the origin; right begins at the split beat.
        assert_eq!(left.start_beat, 4.0);
        assert_eq!(left.duration_beats, 4.0);
        assert_eq!(right.start_beat, 8.0);
        assert_eq!(right.duration_beats, 4.0);
        // Durations still cover the original span with no gap/overlap.
        assert_eq!(
            left.duration_beats + right.duration_beats,
            clip.duration_beats
        );
        // Right's source continues from where the left ended (offset + left len).
        assert_eq!(right.offset_beats, 6.0);
        assert_eq!(left.offset_beats, 2.0);
        assert_eq!(left.stretch.source_start_samples, 0);
        assert_eq!(left.stretch.source_end_samples, 96_000);
        assert_eq!(right.stretch.source_start_samples, 96_000);
        assert_eq!(right.stretch.source_end_samples, 192_000);
        // Fresh, distinct ids for both halves.
        assert_ne!(left.id, right.id);
        assert_ne!(left.id, clip.id);
    }

    #[test]
    fn split_of_a_stretched_clip_splits_the_source_through_the_ratio() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        // Four seconds of source played at 200%: 16 beats at 120 BPM.
        let mut clip = audio_clip("clip-5", 0.0, 16.0, 0.0);
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = 192_000;
        clip.stretch.source_end_samples = 192_000;
        clip.stretch = clip.stretch.with_timing(StretchTiming::Speed, 120.0);
        clip.stretch.set_stretch_ratio(2.0);
        let (left, right) = state
            .plan_audio_clip_split(&clip, 8.0)
            .expect("split inside the clip");
        // Half the timeline is half the source, not the whole take twice.
        assert_eq!(left.stretch.source_end_samples, 96_000);
        assert_eq!(right.stretch.source_start_samples, 96_000);
        assert_eq!(right.stretch.source_end_samples, 192_000);
    }

    /// A 4 s, 48 kHz clip at 120 BPM (8 beats), decoded and on a track.
    fn state_with_decoded_clip() -> (TimelineState, String) {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let id = state.import_audio_at("C:/a/loop.wav".to_string(), "loop".to_string(), 0.0, 1.0e9);
        state.update_audio_clip_metadata("C:/a/loop.wav", "wav", 48_000, 2, 192_000, 4.0);
        (state, id)
    }

    #[test]
    fn stretch_tool_drag_changes_the_ratio_and_keeps_the_source_window() {
        let (mut state, id) = state_with_decoded_clip();
        assert!(state.stretch_clip_edge(&id, ClipEdge::Right, 16.0));
        let (_, clip) = state.find_clip(&id).unwrap();
        assert!((clip.duration_beats - 16.0).abs() < 1e-3);
        assert_eq!(clip.stretch.timing(), StretchTiming::Speed);
        assert!(clip.stretch.keeps_pitch());
        assert!((clip.stretch.stretch_ratio - 2.0).abs() < 1e-6);
        assert_eq!(clip.stretch.source_start_samples, 0);
        assert_eq!(clip.stretch.source_end_samples, 192_000);
        // The model and the drawing agree on the new length.
        assert!(!state.reconcile_audio_clip_lengths());

        // Left edge: right edge stays put.
        assert!(state.stretch_clip_edge(&id, ClipEdge::Left, 12.0));
        let (_, clip) = state.find_clip(&id).unwrap();
        assert!((clip.start_beat - 12.0).abs() < 1e-3);
        assert!((clip.start_beat + clip.duration_beats - 16.0).abs() < 1e-3);
        assert!((clip.stretch.stretch_ratio - 0.5).abs() < 1e-6);
    }

    #[test]
    fn edge_trim_of_a_stretched_clip_moves_the_window_not_the_speed() {
        let (mut state, id) = state_with_decoded_clip();
        assert!(state.stretch_clip_edge(&id, ClipEdge::Right, 16.0));
        // Trim the 16-beat, 200% clip back to 8 beats: half the source.
        assert!(state.resize_clip(&id, ClipEdge::Right, 8.0));
        let (_, clip) = state.find_clip(&id).unwrap();
        assert!((clip.stretch.stretch_ratio - 2.0).abs() < 1e-6);
        assert_eq!(clip.stretch.source_end_samples, 96_000);
        assert!((clip.duration_beats - 8.0).abs() < 1e-3);
        // And it survives the length re-derivation instead of snapping back.
        state.reconcile_audio_clip_lengths();
        let (_, clip) = state.find_clip(&id).unwrap();
        assert!((clip.duration_beats - 8.0).abs() < 1e-3);
    }

    #[test]
    fn metadata_update_does_not_resize_trimmed_siblings() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        let left_id =
            state.import_audio_at("C:/a/loop.wav".to_string(), "loop".to_string(), 0.0, 1.0e9);
        state.update_audio_clip_metadata("C:/a/loop.wav", "wav", 48_000, 2, 192_000, 4.0);
        let snapshot = state
            .find_clip(&left_id)
            .map(|(_, clip)| clip.clone())
            .unwrap();
        let Some((left, right)) = state.plan_audio_clip_split(&snapshot, snapshot.start_beat + 2.0)
        else {
            panic!("split should succeed");
        };
        state.delete_clip(&left_id);
        let left_len = left.duration_beats;
        let right_len = right.duration_beats;
        let right_start = right.stretch.source_start_samples;
        if let Some(track) = state.tracks.first_mut() {
            track.clips.push(left);
            track.clips.push(right);
        }

        state.update_audio_clip_metadata("C:/a/loop.wav", "wav", 48_000, 2, 192_000, 4.0);

        let clips: Vec<_> = state.tracks[0].clips.iter().cloned().collect();
        assert_eq!(clips.len(), 2);
        assert!((clips[0].duration_beats - left_len).abs() < 1e-4);
        assert!((clips[1].duration_beats - right_len).abs() < 1e-4);
        assert_eq!(clips[1].stretch.source_start_samples, right_start);
    }

    #[test]
    fn split_is_a_noop_near_edges_and_for_non_audio() {
        let state = TimelineState::default();
        let clip = audio_clip("clip-1", 0.0, 4.0, 0.0);
        // Within MIN_CLIP_SPLIT_BEATS of either edge → no split.
        assert!(state.plan_audio_clip_split(&clip, 0.1).is_none());
        assert!(state.plan_audio_clip_split(&clip, 3.95).is_none());
        // Exactly on an edge → no split.
        assert!(state.plan_audio_clip_split(&clip, 0.0).is_none());

        let mut midi = clip.clone();
        midi.clip_type = ClipType::Midi {
            notes: Vec::new(),
            controller_lanes: Vec::new(),
            sysex_events: Vec::new(),
            articulations: Vec::new(),
        };
        assert!(state.plan_audio_clip_split(&midi, 2.0).is_none());
    }

    #[test]
    fn next_clip_id_after_increments_numeric_suffix() {
        let state = TimelineState::default();
        assert_eq!(state.next_clip_id_after("clip-7"), "clip-8");
        // Non-numeric ids fall back to a stable, distinct suffix.
        assert_eq!(state.next_clip_id_after("weird"), "weird-split");
    }
}

#[cfg(test)]
mod lane_origin_tests {
    use super::*;

    /// The browser panel is collapsible, so the timeline's window-space origin
    /// moves with it. A hardcoded panel width made every window-x → beat
    /// mapping (clip edge-resize, clip move, ruler scrub, lane tools) land a
    /// full panel width away once the browser was hidden.
    #[test]
    fn window_x_to_beats_follows_the_collapsible_browser_panel() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.viewport.pixels_per_second = 150.0;
        state.sync_pixels_per_beat();
        let ppb = state.viewport.pixels_per_beat;

        // Browser panel open: the lane starts past the panel and the headers.
        state.viewport.panel_origin_x = 272.0;
        assert_eq!(state.lane_origin_x(), 272.0 + HEADER_WIDTH);
        let open_x = 272.0 + HEADER_WIDTH + 4.0 * ppb;
        assert!((state.beats_from_window_x(open_x) - 4.0).abs() < 0.001);

        // Browser panel hidden: the same beat now sits a panel width left.
        state.viewport.panel_origin_x = 0.0;
        assert_eq!(state.lane_origin_x(), HEADER_WIDTH);
        let hidden_x = HEADER_WIDTH + 4.0 * ppb;
        assert!((state.beats_from_window_x(hidden_x) - 4.0).abs() < 0.001);
    }

    /// A right-edge trim driven from a window-space pointer must land on the
    /// pointer's beat with the browser panel collapsed, not a panel width past
    /// it.
    #[test]
    fn audio_right_edge_trim_lands_on_pointer_beat_with_browser_hidden() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.snap_to_grid = false;
        state.viewport.pixels_per_second = 150.0;
        state.sync_pixels_per_beat();
        state.viewport.panel_origin_x = 0.0;

        let clip_id =
            state.import_audio_at("C:/a/loop.wav".to_string(), "loop".to_string(), 0.0, 1.0e9);
        state.update_audio_clip_metadata("C:/a/loop.wav", "wav", 48_000, 2, 48_000, 1.0);

        let pointer_x = state.lane_origin_x() + 0.75 * state.viewport.pixels_per_beat;
        let beat = state.beats_from_window_x(pointer_x);
        assert!(state.resize_clip(&clip_id, ClipEdge::Right, beat));

        let clip = state.find_clip(&clip_id).map(|(_, clip)| clip).unwrap();
        assert!(
            (clip.duration_beats - 0.75).abs() < 0.001,
            "trim followed the wrong coordinate space: {}",
            clip.duration_beats
        );
    }

    /// The lightweight per-frame gesture context exists so lane/clip/ruler
    /// closures stop deep-cloning the whole project. It is only safe to swap in
    /// if it resolves pointer coordinates and snapping bit-for-bit like the
    /// state it replaced.
    #[test]
    fn gesture_context_matches_timeline_state_coordinate_and_snap_math() {
        let mut state = TimelineState::default();
        state.bpm = 137.0;
        state.viewport.pixels_per_second = 92.0;
        state.viewport.scroll_x = 431.0;
        state.viewport.panel_origin_x = 210.0;
        state.sync_pixels_per_beat();
        state.snap_to_grid = true;
        state.grid_division = SnapDivision::Div1_8;
        // A meter change mid-timeline: bar-relative snapping must follow the
        // marker in force at the snapped beat, not the one at the playhead.
        state.add_time_signature_point(0.0, 4, 4);
        state.add_time_signature_point(16.0, 7, 8);

        let ctx = state.gesture_context();
        for x in [-40.0_f32, 0.0, 17.3, 250.0, 999.0, 4321.0] {
            assert_eq!(ctx.x_to_beats(x), state.x_to_beats(x), "x_to_beats @ {x}");
            assert_eq!(ctx.x_to_beat(x), state.x_to_beat(x), "x_to_beat @ {x}");
            assert_eq!(
                ctx.lane_x_from_window_x(x),
                state.lane_x_from_window_x(x),
                "lane_x @ {x}"
            );
            assert_eq!(
                ctx.beats_from_window_x(x),
                state.beats_from_window_x(x),
                "beats_from_window_x @ {x}"
            );
        }
        for beat in [0.0_f32, 0.13, 3.9, 15.99, 16.4, 33.2] {
            assert_eq!(
                ctx.snap_beats(beat),
                state.snap_beats(beat),
                "snap @ {beat}"
            );
            assert_eq!(
                ctx.snap_beats_with_bypass(beat, true),
                state.snap_beats_with_bypass(beat, true),
                "snap bypass @ {beat}"
            );
            assert_eq!(
                ctx.beats_to_x(beat),
                state.beats_to_x(beat),
                "beats_to_x @ {beat}"
            );
        }
        for seconds in [0.0_f32, 0.4, 2.7, 11.0] {
            assert_eq!(
                ctx.snap_time(seconds),
                state.snap_time(seconds),
                "snap_time @ {seconds}"
            );
        }
        assert_eq!(ctx.seconds_per_beat(), state.seconds_per_beat());
        assert_eq!(ctx.lane_origin_x(), state.lane_origin_x());
        assert_eq!(
            ctx.arrangement_content_top(),
            state.arrangement_content_top()
        );
    }

    /// The meter path resolves one id per published meter against the track
    /// list every tick. That batch must stay linear: at the scale multi-output
    /// VSTi projects reach (thousands of channels) a per-meter linear scan is
    /// quadratic and eats the UI thread at the display refresh.
    #[test]
    fn track_index_by_id_resolves_every_track_exactly_once() {
        let mut state = TimelineState::default();
        for index in 0..64 {
            state.create_track(CreateTrackOptions {
                track_type: TrackType::Instrument,
                name: format!("Track {index}"),
                color: crate::theme::Colors::track_color_for_index(index),
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
        }

        let index_by_id = state.track_index_by_id();
        assert_eq!(index_by_id.len(), state.tracks.len(), "one entry per track");
        for (expected_index, track) in state.tracks.iter().enumerate() {
            assert_eq!(
                index_by_id.get(track.id.as_str()).copied(),
                Some(expected_index),
                "id {} must map to its own slot",
                track.id
            );
            // The map must agree with the linear lookup it replaces.
            assert_eq!(
                state.find_track(&track.id).map(|t| t.id.as_str()),
                Some(track.id.as_str())
            );
        }
        assert_eq!(index_by_id.get("no-such-track").copied(), None);
    }
}

// ── Solfege pitch expression: stable-id association and undo ────────────────
//
// These cover the editor contract that the MIDI tab and the Pitch tab operate
// on one set of notes: pitch curves are keyed to a note's stable id, ride the
// shared `EditMidiNotes` history, and survive musical edits.

#[cfg(test)]
mod solfege_pitch_expression_tests {
    use super::midi_edit_tests::{note, state_with_midi_clip};
    use super::*;
    use crate::components::edit::EditCommand;
    use sphere_midi_service::{ExpressionCurve, ExpressionPoint};

    fn scooped_curve() -> PitchCurve {
        PitchCurve::from_points(vec![
            PitchPoint::new(0.0, -100.0, PitchSegmentShape::Smooth),
            PitchPoint::new(0.5, 0.0, PitchSegmentShape::Linear),
        ])
    }

    fn clip_with_expressive_note() -> (TimelineState, String, u64) {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state
            .add_midi_note(&clip_id, 60, 0.0, 2.0, 100)
            .expect("note added");
        let notes = state.midi_clip_notes_mut(&clip_id).unwrap();
        let note = notes.iter_mut().find(|n| n.id == id).unwrap();
        note.pitch_curve = Some(scooped_curve());
        note.expression.pitch = ExpressionCurve::from_points(vec![
            ExpressionPoint::new(0.0, 0.0),
            ExpressionPoint::new(1.0, 0.5),
        ]);
        (state, clip_id, id)
    }

    #[test]
    fn a_pitch_curve_is_reached_by_note_id_not_by_index() {
        let (mut state, clip_id, id) = clip_with_expressive_note();
        // Insert a note that sorts before the expressive one, moving its index.
        let _ = state.add_midi_note(&clip_id, 48, 0.0, 1.0, 100);
        let curve = state.note_pitch_curve(&clip_id, id);
        assert_eq!(curve.len(), 2);
        assert!((curve.cents_at(0.0) + 100.0).abs() < 0.001);
    }

    #[test]
    fn transposing_preserves_the_relative_pitch_expression() {
        let (mut state, clip_id, id) = clip_with_expressive_note();
        let before = state.note_pitch_curve(&clip_id, id);
        state.transpose_midi_notes(&clip_id, &[id], 2);
        let after = state.note_pitch_curve(&clip_id, id);
        assert_eq!(before.points.len(), after.points.len());
        for (a, b) in before.points.iter().zip(&after.points) {
            assert_eq!(a.cents, b.cents);
            assert_eq!(a.beat, b.beat);
        }
        let note = note(&state, &clip_id, id);
        assert_eq!(note.pitch, 62);
        // The sounding pitch moved by exactly the transposition.
        assert!(
            (TimelineState::note_sounding_pitch(&note, 0.0) - 61.0).abs() < 0.001,
            "a scoop under D4 must sound a semitone under D4"
        );
    }

    #[test]
    fn moving_a_note_keeps_its_note_relative_curve() {
        let (mut state, clip_id, id) = clip_with_expressive_note();
        state.move_midi_notes(&clip_id, &[(id, 1.5, 60)]);
        let curve = state.note_pitch_curve(&clip_id, id);
        assert!((curve.cents_at(0.0) + 100.0).abs() < 0.001);
    }

    #[test]
    fn edit_midi_notes_undoes_and_redoes_a_pitch_curve_edit() {
        let (mut state, clip_id, id) = clip_with_expressive_note();
        let prev = state.midi_note_snapshot(&clip_id, id).unwrap();
        let mut next = prev.clone();
        let mut curve = next.pitch_curve.clone().unwrap();
        curve.set_point(1.0, 42.0, PitchSegmentShape::Linear, 0.0);
        next.pitch_curve = Some(curve);

        let cmd = EditCommand::EditMidiNotes {
            clip_id: clip_id.clone(),
            prev: vec![prev.clone()],
            next: vec![next.clone()],
        };
        cmd.execute(&mut state);
        assert_eq!(state.note_pitch_curve(&clip_id, id).len(), 3);
        assert_eq!(
            state
                .midi_note(&clip_id, id)
                .unwrap()
                .expression
                .pitch
                .points[1]
                .value,
            0.5
        );

        cmd.undo(&mut state);
        assert_eq!(state.note_pitch_curve(&clip_id, id).len(), 2);
        assert_eq!(
            state
                .midi_note(&clip_id, id)
                .unwrap()
                .expression
                .pitch
                .points[1]
                .value,
            0.5
        );

        cmd.execute(&mut state);
        assert!((state.note_pitch_curve(&clip_id, id).cents_at(1.0) - 42.0).abs() < 0.001);
    }

    #[test]
    fn deleting_and_undoing_restores_the_expression() {
        let (mut state, clip_id, id) = clip_with_expressive_note();
        let snapshot = state.midi_note_snapshot(&clip_id, id).unwrap();
        let cmd = EditCommand::DeleteMidiNotes {
            clip_id: clip_id.clone(),
            notes: vec![snapshot],
        };
        cmd.execute(&mut state);
        assert!(state.midi_note(&clip_id, id).is_none());
        cmd.undo(&mut state);
        assert_eq!(state.note_pitch_curve(&clip_id, id).len(), 2);
    }

    #[test]
    fn an_untouched_note_sounds_at_its_notated_pitch() {
        let (mut state, clip_id) = state_with_midi_clip();
        let id = state
            .add_midi_note(&clip_id, 67, 0.0, 1.0, 100)
            .expect("note added");
        let note = note(&state, &clip_id, id);
        assert!(note.pitch_curve.is_none());
        assert_eq!(TimelineState::note_sounding_pitch(&note, 0.4), 67.0);
    }
}

/// The conductor lanes had a fixed height and a collapse toggle, with no way to
/// give the Tempo curve (or the Song Text rows) more room. These cover the drag
/// state machine and the geometry that depends on it.
#[cfg(test)]
mod global_lane_resize_tests {
    use super::*;

    fn drag(state: &mut TimelineState, kind: GlobalLaneKind, from_y: f32, to_y: f32) {
        state.arm_global_lane_resize(kind, from_y);
        assert!(
            state.ensure_global_lane_resize_from_arm(to_y),
            "the first drag-move must promote the armed handle to a live resize"
        );
    }

    #[test]
    fn dragging_down_makes_the_tempo_lane_taller() {
        let mut state = TimelineState::default();
        let start = state.tempo_track_height();
        drag(&mut state, GlobalLaneKind::Tempo, 100.0, 160.0);
        assert!(
            (state.tempo_track_height() - (start + 60.0)).abs() < 0.01,
            "height must follow the absolute pointer delta"
        );
        assert!(state.finish_global_lane_resize().is_some());
    }

    /// Every window-y -> arrangement-y conversion is built on the conductor
    /// block's total height, so a resized lane has to move the content top with
    /// it or all arrangement hit-testing shifts.
    #[test]
    fn a_resized_lane_moves_the_arrangement_content_top() {
        let mut state = TimelineState::default();
        let before = state.arrangement_content_top();
        drag(&mut state, GlobalLaneKind::Tempo, 0.0, 50.0);
        state.finish_global_lane_resize();
        assert!(
            (state.arrangement_content_top() - (before + 50.0)).abs() < 0.01,
            "the arrangement must start below the taller lane"
        );
    }

    #[test]
    fn the_drag_is_clamped_at_both_ends() {
        let mut state = TimelineState::default();
        drag(&mut state, GlobalLaneKind::Tempo, 0.0, 5000.0);
        assert!((state.tempo_track_height() - GLOBAL_LANE_MAX_HEIGHT).abs() < 0.01);
        state.finish_global_lane_resize();

        drag(&mut state, GlobalLaneKind::Tempo, 0.0, -5000.0);
        assert!((state.tempo_track_height() - GLOBAL_LANE_MIN_HEIGHT).abs() < 0.01);
    }

    /// Dragging a collapsed lane has to un-collapse it, or the collapse flag
    /// would keep winning and the lane would snap back on release.
    #[test]
    fn dragging_a_collapsed_lane_expands_it() {
        let mut state = TimelineState::default();
        state.tempo_track_collapsed = true;
        drag(&mut state, GlobalLaneKind::Tempo, 0.0, 40.0);
        assert!(!state.tempo_track_collapsed);
        assert!(state.tempo_track_height() > TEMPO_TRACK_HEIGHT_COLLAPSED);
    }

    #[test]
    fn escape_puts_the_lane_back_where_it_started() {
        let mut state = TimelineState::default();
        let start = state.tempo_track_height();
        drag(&mut state, GlobalLaneKind::Tempo, 0.0, 70.0);
        assert!(state.cancel_global_lane_resize());
        assert!((state.tempo_track_height() - start).abs() < 0.01);
        assert!(
            state
                .global_lane_heights
                .get(GlobalLaneKind::Tempo)
                .is_none(),
            "a cancelled drag on a default-height lane must leave it on the default"
        );
    }

    /// A drag that ends where it began is not an edit; it must not push a dead
    /// undo step.
    #[test]
    fn a_zero_delta_drag_records_nothing() {
        let mut state = TimelineState::default();
        state.arm_global_lane_resize(GlobalLaneKind::Tempo, 120.0);
        state.ensure_global_lane_resize_from_arm(120.0);
        assert!(state.finish_global_lane_resize().is_none());
    }

    #[test]
    fn the_undo_pair_restores_the_default_rather_than_pinning_it() {
        let mut state = TimelineState::default();
        drag(&mut state, GlobalLaneKind::TimeSignature, 0.0, 30.0);
        let (prev, next) = state
            .finish_global_lane_resize()
            .expect("a moved drag is an edit");
        assert!(prev.get(GlobalLaneKind::TimeSignature).is_none());
        assert!(next.get(GlobalLaneKind::TimeSignature).is_some());
    }

    #[test]
    fn double_click_reset_returns_to_the_default_height() {
        let mut state = TimelineState::default();
        drag(&mut state, GlobalLaneKind::SongText, 0.0, 60.0);
        state.finish_global_lane_resize();
        assert!(state
            .reset_global_lane_height(GlobalLaneKind::SongText)
            .is_some());
        assert!(
            state
                .reset_global_lane_height(GlobalLaneKind::SongText)
                .is_none(),
            "resetting an already-default lane is not an edit"
        );
    }

    /// Resizing one lane must not disturb its neighbours' geometry.
    #[test]
    fn resizing_one_lane_leaves_the_others_alone() {
        let mut state = TimelineState::default();
        let ts_before = state.time_signature_track_height();
        drag(&mut state, GlobalLaneKind::Tempo, 0.0, 40.0);
        state.finish_global_lane_resize();
        assert!((state.time_signature_track_height() - ts_before).abs() < 0.01);
    }

    /// A hidden lane still contributes 0 to the conductor block, custom height
    /// or not.
    #[test]
    fn a_hidden_lane_contributes_no_height() {
        let mut state = TimelineState::default();
        drag(&mut state, GlobalLaneKind::Tempo, 0.0, 60.0);
        state.finish_global_lane_resize();
        state.hide_tempo_track_lane();
        assert_eq!(state.tempo_track_height(), 0.0);
    }
}

/// Markers and regions used to exist only as decoration inside the ruler. These
/// cover the state the dedicated lanes are built on: hit-testing, moving,
/// selection hygiene, and the conductor-block geometry the two extra lanes
/// change for everyone else.
#[cfg(test)]
mod marker_region_lane_tests {
    use super::*;

    fn state_at_zoom(pixels_per_beat: f32) -> TimelineState {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_beat = pixels_per_beat;
        state
    }

    #[test]
    fn both_structure_lanes_are_visible_by_default() {
        let state = TimelineState::default();
        let lanes = state.visible_global_lanes();
        assert_eq!(
            lanes,
            vec![
                GlobalLaneKind::Arranger,
                GlobalLaneKind::Marker,
                GlobalLaneKind::Tempo,
                GlobalLaneKind::TimeSignature,
            ],
            "structure lanes sit above the conductor data lanes"
        );
    }

    /// Every window-y -> arrangement-y conversion is built on the conductor
    /// block, so two new lanes have to be inside that total or the arrangement
    /// paints over them.
    #[test]
    fn the_new_lanes_are_counted_in_the_conductor_block() {
        let mut state = TimelineState::default();
        let with_both = state.global_lanes_height();
        state.hide_marker_track_lane();
        state.hide_region_track_lane();
        let without = state.global_lanes_height();
        assert!(
            (with_both - without - MARKER_TRACK_HEIGHT - REGION_TRACK_HEIGHT).abs() < 0.01,
            "hiding both lanes must give back exactly their height"
        );
    }

    /// The Tempo lane maps a pointer y to a BPM, so it can only be correct if
    /// its origin follows the lanes stacked above it.
    #[test]
    fn the_tempo_lane_knows_it_is_no_longer_first() {
        let state = TimelineState::default();
        let top = state.global_lane_top(GlobalLaneKind::Tempo);
        assert!(
            (top - (REGION_TRACK_HEIGHT + MARKER_TRACK_HEIGHT)).abs() < 0.01,
            "tempo starts below the region and marker lanes, got {top}"
        );
        assert_eq!(state.global_lane_top(GlobalLaneKind::Arranger), 0.0);
    }

    #[test]
    fn a_hidden_lane_drops_out_of_the_offsets() {
        let mut state = TimelineState::default();
        state.hide_region_track_lane();
        assert!((state.global_lane_top(GlobalLaneKind::Tempo) - MARKER_TRACK_HEIGHT).abs() < 0.01);
    }

    #[test]
    fn marker_hit_test_picks_the_nearest_inside_the_slop() {
        let mut state = state_at_zoom(40.0);
        let near = state.add_marker_at_beat(4.0);
        state.add_marker_at_beat(12.0);
        // 0.25 beats at 40 px/beat is 10 px — inside the lane's grab slop.
        assert_eq!(state.marker_at(4.2, 0.25).as_deref(), Some(near.as_str()));
        assert_eq!(state.marker_at(8.0, 0.25), None, "the gap is not a hit");
    }

    #[test]
    fn marker_hit_test_breaks_a_tie_on_distance() {
        let mut state = state_at_zoom(40.0);
        let left = state.add_marker_at_beat(4.0);
        let right = state.add_marker_at_beat(5.0);
        assert_eq!(state.marker_at(4.1, 2.0).as_deref(), Some(left.as_str()));
        assert_eq!(state.marker_at(4.9, 2.0).as_deref(), Some(right.as_str()));
    }

    #[test]
    fn moving_a_marker_keeps_the_list_beat_ordered() {
        let mut state = TimelineState::default();
        let first = state.add_marker_at_beat(2.0);
        state.add_marker_at_beat(6.0);
        assert!(state.move_marker(&first, 10.0));
        assert!(
            state.markers[0].beat <= state.markers[1].beat,
            "the lane, the ruler, and MIDI export all read this order"
        );
        assert!(
            !state.move_marker(&first, 10.0),
            "a move that changes nothing is not an edit"
        );
    }

    #[test]
    fn a_marker_cannot_be_dragged_before_the_song_start() {
        let mut state = TimelineState::default();
        let id = state.add_marker_at_beat(4.0);
        assert!(state.move_marker(&id, -8.0));
        assert_eq!(state.marker(&id).unwrap().beat, 0.0);
    }

    #[test]
    fn deleting_clears_a_selection_that_named_it() {
        let mut state = TimelineState::default();
        let id = state.add_marker_at_beat(4.0);
        state.select_marker(&id);
        assert!(state.delete_marker(&id));
        assert!(
            state.selected_marker_id.is_none(),
            "a selection pointing at a deleted marker would outlive it"
        );
    }

    #[test]
    fn hiding_the_lane_drops_its_selection() {
        let mut state = TimelineState::default();
        let id = state.add_region_at_beat(0.0);
        state.select_region(&id);
        state.hide_region_track_lane();
        assert!(state.selected_region_id.is_none());
    }

    #[test]
    fn region_hit_test_covers_the_whole_span() {
        let mut state = TimelineState::default();
        let id = state.add_region_at_beat(4.0);
        let (start, end) = state.region(&id).unwrap().normalized_range();
        assert_eq!(state.region_at(start).as_deref(), Some(id.as_str()));
        assert_eq!(
            state.region_at((start + end) * 0.5).as_deref(),
            Some(id.as_str())
        );
        assert_eq!(state.region_at(end + 1.0), None);
    }

    #[test]
    fn moving_a_region_keeps_its_length() {
        let mut state = TimelineState::default();
        let id = state.add_region_at_beat(4.0);
        let (start, end) = state.region(&id).unwrap().normalized_range();
        let length = end - start;
        assert!(state.move_region(&id, 16.0));
        let (moved_start, moved_end) = state.region(&id).unwrap().normalized_range();
        assert!((moved_start - 16.0).abs() < 1.0e-9);
        assert!(
            (moved_end - moved_start - length).abs() < 1.0e-9,
            "a move is not a resize"
        );
    }

    /// Trimming the right edge past the left one used to be able to produce a
    /// zero-width block that could never be grabbed again.
    #[test]
    fn a_region_cannot_be_trimmed_to_nothing() {
        let mut state = TimelineState::default();
        let id = state.add_region_at_beat(4.0);
        state.update_region_range(&id, 4.0, 4.0);
        let (start, end) = state.region(&id).unwrap().normalized_range();
        assert!(
            end - start >= MIN_REGION_BEATS - 1.0e-9,
            "got {}",
            end - start
        );
    }

    #[test]
    fn the_lane_headers_report_what_is_in_them() {
        let mut state = TimelineState::default();
        assert_eq!(state.marker_lane_header_subtitle(), "No markers");
        state.add_marker_at_beat(0.0);
        assert_eq!(state.marker_lane_header_subtitle(), "Marker 1");
        state.add_marker_at_beat(8.0);
        assert_eq!(state.marker_lane_header_subtitle(), "2 markers");

        assert_eq!(state.region_lane_header_subtitle(), "No regions");
        state.add_region_at_beat(0.0);
        assert_eq!(state.region_lane_header_subtitle(), "Region 1");
    }

    /// The lanes are resizable like the rest of the conductor block.
    #[test]
    fn the_structure_lanes_resize_like_the_others() {
        let mut state = TimelineState::default();
        state.arm_global_lane_resize(GlobalLaneKind::Marker, 0.0);
        assert!(state.ensure_global_lane_resize_from_arm(45.0));
        assert!((state.marker_track_height() - (MARKER_TRACK_HEIGHT + 45.0)).abs() < 0.01);
        assert!(state.finish_global_lane_resize().is_some());

        state.arm_global_lane_resize(GlobalLaneKind::Arranger, 0.0);
        assert!(state.ensure_global_lane_resize_from_arm(20.0));
        assert!((state.region_track_height() - (REGION_TRACK_HEIGHT + 20.0)).abs() < 0.01);
    }
}

/// Pointer maths used to run on three different transforms at once: the
/// constants `SIDEBAR_WIDTH + HEADER_WIDTH` (ruler click, meter lane, track
/// lane), `panel_origin_x + HEADER_WIDTH` (tempo, song text), and the
/// element's real `bounds.origin` (ruler scrub, region drag). Only the last
/// was right, which is why dragging the playhead landed where you pointed and
/// clicking it did not. These pin the single transform everything now shares.
#[cfg(test)]
mod lane_pointer_transform_tests {
    use super::*;

    #[test]
    fn a_measured_origin_wins_over_the_chrome_estimate() {
        let mut state = TimelineState::default();
        state.viewport.panel_origin_x = 0.0;
        assert_eq!(
            state.lane_origin_x(),
            HEADER_WIDTH,
            "with nothing measured yet the estimate still has to be usable"
        );

        // What the shell actually draws: a left rail the chrome estimate knows
        // nothing about.
        state.viewport.lane_origin_x_measured = Some(HEADER_WIDTH + 38.0);
        assert_eq!(state.lane_origin_x(), HEADER_WIDTH + 38.0);
    }

    /// A click and the drag that follows it must resolve to the same beat.
    #[test]
    fn click_and_drag_resolve_the_same_beat() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_beat = 75.0;
        state.viewport.scroll_x = 0.0;
        state.viewport.lane_origin_x_measured = Some(358.0);

        // The scrub path works in element-local x (`event.bounds.origin`); the
        // click path works in window x. Same pixel, same beat.
        let window_x = 358.0 + 150.0;
        let from_click = state.x_to_beats(state.lane_x_from_window_x(window_x));
        let from_scrub = state.x_to_beats(150.0);
        assert!((from_click - from_scrub).abs() < 1.0e-6);
        assert!((from_click - 2.0).abs() < 1.0e-6);
    }

    #[test]
    fn the_gesture_context_carries_the_measured_origin() {
        let mut state = TimelineState::default();
        state.viewport.lane_origin_x_measured = Some(412.0);
        let gesture = TimelineGestureContext::from_state(&state);
        assert_eq!(
            gesture.lane_origin_x(),
            state.lane_origin_x(),
            "closures that captured only the frame geometry must not drift from the state"
        );
    }

    /// Drawing and hit-testing have to be inverses, or a marker is grabbable
    /// somewhere other than where it is painted.
    #[test]
    fn window_x_round_trips_through_the_drawn_position() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_beat = 64.0;
        state.viewport.scroll_x = 220.0;
        state.viewport.lane_origin_x_measured = Some(358.0);

        for beat in [0.0_f32, 3.5, 12.25, 41.0] {
            let drawn_window_x = state.beats_to_x(beat) + state.lane_origin_x();
            let hit_beat = state.x_to_beats(state.lane_x_from_window_x(drawn_window_x));
            assert!(
                (hit_beat - beat).abs() < 0.02,
                "beat {beat} drew at {drawn_window_x} but hit-tested as {hit_beat}"
            );
        }
    }

    /// The Tempo lane maps pointer y onto a BPM axis, so its origin has to
    /// follow whatever lanes are stacked above it.
    #[test]
    fn the_tempo_lane_origin_follows_the_lanes_above_it() {
        let mut state = TimelineState::default();
        let with_structure = state.tempo_lane_origin_y();

        state.hide_region_track_lane();
        state.hide_marker_track_lane();
        let alone = state.tempo_lane_origin_y();

        assert!(
            (with_structure - alone - MARKER_TRACK_HEIGHT - REGION_TRACK_HEIGHT).abs() < 0.01,
            "the tempo lane moved down by exactly the two lanes above it"
        );
        assert!(
            (alone - (crate::shell_metrics::APP_CHROME_HEIGHT + RULER_HEIGHT)).abs() < 0.01,
            "with nothing above it the tempo lane still starts under the ruler"
        );
    }

    /// Resizing a lane above the Tempo lane moves it too — the old inline
    /// `APP_CHROME_HEIGHT + RULER_HEIGHT` could not see this at all.
    #[test]
    fn resizing_a_lane_above_moves_the_tempo_origin() {
        let mut state = TimelineState::default();
        let before = state.tempo_lane_origin_y();
        state.arm_global_lane_resize(GlobalLaneKind::Marker, 0.0);
        state.ensure_global_lane_resize_from_arm(50.0);
        state.finish_global_lane_resize();
        assert!((state.tempo_lane_origin_y() - before - 50.0).abs() < 0.01);
    }

    /// Where a tempo marker is *drawn* is where clicking it has to resolve its
    /// own BPM.
    ///
    /// Both the lane's hit test and its drag used to subtract `TEMPO_LANE_PAD`
    /// before calling `y_to_bpm`, which already takes the pad off itself. The
    /// axis ended up a pad's height out: the dot sat 5 px above the pointer
    /// that made it, and grabbing one moved it before the drag had begun.
    #[test]
    fn a_tempo_dot_reads_back_the_bpm_it_was_drawn_for() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.add_tempo_point(8.0, 150.0);

        for bpm in [110.0_f64, 120.0, 135.0, 150.0] {
            let drawn_y = state.tempo_window_y_at_bpm(bpm);
            let read_back = state.tempo_bpm_at_window_y(drawn_y);
            assert!(
                (read_back - bpm).abs() < 0.5,
                "a dot drawn for {bpm} BPM at y={drawn_y} hit-tested as {read_back}"
            );
        }
    }

    /// The pad is real padding, not an offset: the top and bottom of the drawn
    /// band still map to the ends of the lane's BPM range.
    #[test]
    fn the_tempo_axis_spans_the_lane_between_its_pads() {
        let state = TimelineState::default();
        let (min_bpm, max_bpm) = state.tempo_lane_bpm_range();
        let top = state.tempo_lane_origin_y() + TEMPO_LANE_PAD;
        let bottom = state.tempo_lane_origin_y() + state.tempo_track_height() - TEMPO_LANE_PAD;
        assert!((state.tempo_bpm_at_window_y(top) - max_bpm).abs() < 0.5);
        assert!((state.tempo_bpm_at_window_y(bottom) - min_bpm).abs() < 0.5);
    }

    /// A conductor flag is a body running *right* from its beat line, so the
    /// label — the obvious place to aim — has to be part of the target. The
    /// lanes used to hit-test a symmetric ±10 px window around the beat, which
    /// made every click on a flag's name read as "empty lane".
    #[test]
    fn a_flag_is_grabbable_across_its_whole_body() {
        use crate::components::timeline::marker_flag::{flag_hit_index, MARKER_FLAG_HIT_SLOP};

        let spans = [(100.0_f32, 60.0_f32), (300.0, 40.0)];
        assert_eq!(flag_hit_index(&spans, 100.0, MARKER_FLAG_HIT_SLOP), Some(0));
        assert_eq!(
            flag_hit_index(&spans, 150.0, MARKER_FLAG_HIT_SLOP),
            Some(0),
            "the far end of the body is still the flag"
        );
        assert_eq!(
            flag_hit_index(&spans, 97.0, MARKER_FLAG_HIT_SLOP),
            Some(0),
            "a few pixels left of the beat line is still the flag"
        );
        assert_eq!(flag_hit_index(&spans, 200.0, MARKER_FLAG_HIT_SLOP), None);
        assert_eq!(flag_hit_index(&spans, 320.0, MARKER_FLAG_HIT_SLOP), Some(1));
    }

    /// A flag grabbed by its label keeps that offset for the whole move.
    ///
    /// The Marker lane's move resolves the pointer against the marker's own
    /// beat, then snaps — not the other way round. Snapping the pointer first
    /// puts the *cursor* on the grid and leaves the marker wherever the grab
    /// offset happened to land it.
    #[test]
    fn a_marker_grabbed_by_its_label_keeps_its_grab_offset() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_beat = 40.0;
        state.viewport.scroll_x = 0.0;
        state.snap_to_grid = false;

        // Grabbed 1.5 beats into the flag body of a marker sitting at beat 4.
        let grab_offset = 1.5_f64;
        // Pointer dragged to lane x = 8.5 beats -> the marker belongs at 7.
        let landed = state.marker_drag_beat(8.5 * 40.0, grab_offset);
        assert!(
            (landed - 7.0).abs() < 1.0e-6,
            "expected the marker at beat 7, got {landed}"
        );
    }

    /// A move cannot push a marker before the song start, however far left the
    /// pointer goes.
    #[test]
    fn a_marker_move_clamps_at_the_song_start() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_beat = 40.0;
        state.snap_to_grid = false;
        assert_eq!(state.marker_drag_beat(-400.0, 2.0), 0.0);
    }

    /// Where two flags overlap you get the one painted on top, which is the
    /// one you can actually see.
    #[test]
    fn overlapping_flags_resolve_to_the_one_on_top() {
        use crate::components::timeline::marker_flag::flag_hit_index;

        let spans = [(100.0_f32, 120.0_f32), (140.0, 40.0)];
        assert_eq!(flag_hit_index(&spans, 150.0, 0.0), Some(1));
        assert_eq!(flag_hit_index(&spans, 110.0, 0.0), Some(0));
    }
}

#[cfg(test)]
mod global_latch_tests {
    use super::*;

    fn two_audio_tracks(state: &mut TimelineState) -> (String, String) {
        state.tracks.clear();
        (state.create_audio_track(), state.create_audio_track())
    }

    /// The latches are indicators before they are buttons: they must report on
    /// a mute the user cannot see because the track is scrolled away.
    #[test]
    fn a_latch_reports_any_track_not_just_the_visible_ones() {
        let mut state = TimelineState::default();
        let (_first, second) = two_audio_tracks(&mut state);
        assert!(!state.any_track_muted());
        assert!(!state.any_track_soloed());

        assert!(state.set_track_mute(&second, true));
        assert!(state.any_track_muted());
        assert!(!state.any_track_soloed());

        assert!(state.set_track_solo(&second, true));
        assert!(state.any_track_soloed());
    }

    /// Clearing returns exactly the tracks it changed, because the caller sends
    /// one engine param message per id and must not re-send the rest.
    #[test]
    fn clearing_reports_only_the_tracks_it_changed() {
        let mut state = TimelineState::default();
        let (first, second) = two_audio_tracks(&mut state);
        assert!(state.set_track_mute(&first, true));
        assert!(state.set_track_solo(&second, true));

        let unmuted = state.clear_all_track_mutes();
        assert_eq!(unmuted, vec![first.clone()]);
        assert!(!state.any_track_muted());

        let unsoloed = state.clear_all_track_solos();
        assert_eq!(unsoloed, vec![second.clone()]);
        assert!(!state.any_track_soloed());
    }

    /// Pressing a dark latch has nothing to clear, so it must not report work
    /// the caller would turn into engine traffic and a dirty project.
    #[test]
    fn clearing_nothing_reports_nothing() {
        let mut state = TimelineState::default();
        let _ = two_audio_tracks(&mut state);
        assert!(state.clear_all_track_mutes().is_empty());
        assert!(state.clear_all_track_solos().is_empty());
    }
}

#[cfg(test)]
mod per_frame_cost_tests {
    use super::*;

    fn scrolled_state(scroll_x: f32) -> TimelineState {
        let mut state = TimelineState::default();
        state.viewport.scroll_x = scroll_x;
        state
    }

    /// The ruler, the arrangement snapshot and every conductor lane ask for the
    /// same lines with the same arguments, so a default layout used to build
    /// the identical vector six times a frame.
    #[test]
    fn the_same_frame_builds_the_grid_once() {
        let state = scrolled_state(0.0);
        let first = state.arrangement_grid_lines(1200.0);
        let second = state.arrangement_grid_lines(1200.0);
        assert!(
            std::rc::Rc::ptr_eq(&first, &second),
            "the second call must reuse the first build"
        );
    }

    /// Anything the geometry depends on has to invalidate it, or the grid would
    /// stay behind while the arrangement scrolled under it.
    #[test]
    fn moving_the_viewport_rebuilds_the_grid() {
        let a = scrolled_state(0.0);
        let lines_a = a.arrangement_grid_lines(1200.0);
        let b = scrolled_state(500.0);
        let lines_b = b.arrangement_grid_lines(1200.0);
        assert!(!std::rc::Rc::ptr_eq(&lines_a, &lines_b));

        // A different width is a different grid too.
        let lines_c = b.arrangement_grid_lines(900.0);
        assert!(!std::rc::Rc::ptr_eq(&lines_b, &lines_c));
    }

    /// A meter change must invalidate even when nothing about the viewport
    /// moved — bar positions are derived from the map.
    #[test]
    fn a_meter_change_rebuilds_the_grid() {
        let mut state = scrolled_state(0.0);
        let before = state.arrangement_grid_lines(1200.0);
        state.time_signature_map.points = vec![TimeSignaturePoint::with_id("ts-1", 0.0, 7, 8)];
        let after = state.arrangement_grid_lines(1200.0);
        assert!(!std::rc::Rc::ptr_eq(&before, &after));
    }

    /// The cached lines have to be the lines, not a stale or different build.
    #[test]
    fn the_cached_grid_matches_a_fresh_build() {
        let state = scrolled_state(320.0);
        let cached = state.arrangement_grid_lines(1200.0);
        let fresh = state.get_arrangement_grid_lines(1200.0);
        assert_eq!(cached.len(), fresh.len());
        for (a, b) in cached.iter().zip(fresh.iter()) {
            assert_eq!(a.x, b.x);
            assert_eq!(a.level, b.level);
            assert_eq!(a.show_label, b.show_label);
        }
    }

    /// The values accessor exists so the grid loop stops cloning a point (and
    /// its `String` id) once per sub-beat slot; it must agree with the point.
    #[test]
    fn the_meter_values_match_the_meter_point() {
        let mut state = TimelineState::default();
        // No points: the implicit 4/4.
        assert_eq!(
            state.time_signature_map.time_signature_values_at_beat(0.0),
            (4, 4)
        );
        state.time_signature_map.points = vec![
            TimeSignaturePoint::with_id("ts-1", 0.0, 4, 4),
            TimeSignaturePoint::with_id("ts-2", 8.0, 7, 8),
        ];
        for beat in [0.0_f64, 4.0, 7.9, 8.0, 100.0] {
            let point = state.time_signature_map.time_signature_at_beat(beat);
            assert_eq!(
                state.time_signature_map.time_signature_values_at_beat(beat),
                (point.numerator, point.denominator),
                "beat {beat}"
            );
        }
    }

    /// The playback tick calls this for every track on every frame. A track
    /// with no automation still has to follow its own base volume.
    #[test]
    fn a_track_without_automation_still_follows_its_volume() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        // Written straight to the base value, the way a project load does: the
        // fader path already keeps `volume_effective` in step, so it would not
        // exercise the early-out at all.
        if let Some(entry) = state.tracks.iter_mut().find(|t| t.id == track) {
            entry.volume = 0.25;
        }
        assert!(state.recompute_effective_volumes(0.0, "test"));
        let entry = state.find_track(&track).expect("track");
        assert!((entry.volume_effective - 0.25).abs() < 1.0e-6);
        // Nothing moved, so nothing to report the second time.
        assert!(!state.recompute_effective_volumes(4.0, "test"));
    }
}

#[cfg(test)]
mod point_delete_tests {
    use super::*;

    /// Right-click deletes what is under the cursor, which is not necessarily
    /// what is selected — so the by-id delete must take exactly one point.
    #[test]
    fn deleting_an_automation_point_leaves_its_neighbours() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let lane = state
            .ensure_automation_lane(&track, AutomationTarget::TrackVolume)
            .expect("lane");
        let first = state
            .add_automation_point(&track, &lane, 0.0, 0.2)
            .expect("first");
        let second = state
            .add_automation_point(&track, &lane, 4.0, 0.8)
            .expect("second");

        assert!(state.delete_automation_point(&track, &lane, first));
        let points = state
            .automation_lane(&track, &lane)
            .expect("lane")
            .points
            .clone();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].id, second);
    }

    /// A point that is not there is not an error, but it must not report a
    /// deletion — the lane uses that answer to decide whether to swallow the
    /// press, and a false positive would eat the context menu.
    #[test]
    fn deleting_a_missing_automation_point_reports_nothing() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_audio_track();
        let lane = state
            .ensure_automation_lane(&track, AutomationTarget::TrackVolume)
            .expect("lane");
        assert!(!state.delete_automation_point(&track, &lane, 4242));
        assert!(!state.delete_automation_point("no-such-track", &lane, 1));
        assert!(!state.delete_automation_point(&track, "no-such-lane", 1));
    }

    /// The controller lane's delete is by id for the same reason: two points
    /// can share a beat, and only the one under the cursor should go.
    #[test]
    fn deleting_a_controller_point_takes_only_that_point() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let track = state.create_midi_track();
        let clip = state.create_midi_clip(&track, 0.0, 8.0).expect("clip");
        let kind = MidiControllerKind::CC(1);
        assert!(state.ensure_controller_lane(&clip, kind));
        state.put_controller_point(&clip, kind, 1.0, 0.25);
        state.put_controller_point(&clip, kind, 3.0, 0.75);
        let first = state
            .controller_points_snapshot(&clip, kind)
            .first()
            .expect("first point")
            .id;

        assert!(state.delete_controller_point(&clip, kind, first));
        let points = state.controller_points_snapshot(&clip, kind);
        assert_eq!(points.len(), 1);
        assert!(points.iter().all(|p| p.id != first));

        // Already gone: nothing more to delete, and nothing to report.
        assert!(!state.delete_controller_point(&clip, kind, first));
    }
}

/// Per-track timebase: what a clip holds onto when the tempo moves.
mod track_timebase_tests {
    use super::*;

    fn track(state: &mut TimelineState, timebase: TrackTimebase) -> String {
        let id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Midi,
            name: "T".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        state
            .tracks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("track")
            .timebase = timebase;
        id
    }

    fn clip_start(state: &TimelineState, track_id: &str, clip_id: &str) -> f32 {
        state
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .expect("track")
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .expect("clip")
            .start_beat
    }

    /// Halving the tempo doubles the seconds-per-beat. A Musical clip keeps its
    /// beat and moves on the clock; a Linear clip keeps the clock and moves in
    /// beats. Both start at beat 8 = 4s at 120 BPM.
    #[test]
    fn a_tempo_change_moves_linear_clips_in_beats_and_musical_clips_in_seconds() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.bpm = 120.0;

        let musical = track(&mut state, TrackTimebase::Musical);
        let linear = track(&mut state, TrackTimebase::Linear);
        let musical_clip = state.create_midi_clip(&musical, 8.0, 4.0).expect("clip");
        let linear_clip = state.create_midi_clip(&linear, 8.0, 4.0).expect("clip");

        // 8 beats at 120 BPM is 4 seconds.
        assert!((state.seconds_at_beat(8.0) - 4.0).abs() < 1.0e-6);

        let anchors = state.capture_linear_clip_anchors();
        // Only the Linear track's clip is anchored to the clock.
        assert_eq!(anchors.len(), 1);
        state.bpm = 60.0;
        state.reapply_linear_clip_anchors(&anchors);

        // Musical: same beat, and now 8 seconds in.
        assert!((clip_start(&state, &musical, &musical_clip) - 8.0).abs() < 1.0e-4);
        // Linear: still 4 seconds in, which at 60 BPM is beat 4.
        let linear_start = clip_start(&state, &linear, &linear_clip);
        assert!((linear_start - 4.0).abs() < 1.0e-3, "got {linear_start}");
        assert!((state.seconds_at_beat(linear_start as f64) - 4.0).abs() < 1.0e-3);
    }

    /// A Linear clip's musical length follows the tempo too — four beats of
    /// MIDI at 120 BPM is two seconds, and it stays two seconds.
    #[test]
    fn a_linear_midi_clip_keeps_its_wall_clock_length() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.bpm = 120.0;
        let linear = track(&mut state, TrackTimebase::Linear);
        let clip_id = state.create_midi_clip(&linear, 0.0, 4.0).expect("clip");

        let anchors = state.capture_linear_clip_anchors();
        state.bpm = 60.0;
        state.reapply_linear_clip_anchors(&anchors);

        let clip = state.tracks[0]
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .expect("clip");
        // 4 beats @120 = 2s; at 60 BPM that is 2 beats.
        assert!(
            (clip.duration_beats - 2.0).abs() < 1.0e-3,
            "got {}",
            clip.duration_beats
        );
    }

    /// Re-anchoring is exactly reversible, which is what lets undo of a tempo
    /// edit put Linear clips back without carrying a second snapshot.
    #[test]
    fn re_anchoring_round_trips_when_the_tempo_returns() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        state.bpm = 120.0;
        let linear = track(&mut state, TrackTimebase::Linear);
        let clip_id = state.create_midi_clip(&linear, 6.0, 4.0).expect("clip");
        let before = clip_start(&state, &linear, &clip_id);

        let forward = state.capture_linear_clip_anchors();
        state.bpm = 90.0;
        state.reapply_linear_clip_anchors(&forward);
        assert!((clip_start(&state, &linear, &clip_id) - before).abs() > 1.0e-3);

        // Undo: capture under the new tempo, restore the old one, re-anchor.
        let back = state.capture_linear_clip_anchors();
        state.bpm = 120.0;
        state.reapply_linear_clip_anchors(&back);
        assert!((clip_start(&state, &linear, &clip_id) - before).abs() < 1.0e-3);
    }

    /// A project with no Linear track pays nothing and moves nothing.
    #[test]
    fn a_musical_only_project_captures_no_anchors() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let musical = track(&mut state, TrackTimebase::Musical);
        let _ = state.create_midi_clip(&musical, 4.0, 4.0).expect("clip");

        let anchors = state.capture_linear_clip_anchors();
        assert!(anchors.is_empty());
        assert!(!state.reapply_linear_clip_anchors(&anchors));
    }

    #[test]
    fn timebase_tags_round_trip_and_unknown_tags_stay_musical() {
        for timebase in [TrackTimebase::Musical, TrackTimebase::Linear] {
            assert_eq!(TrackTimebase::from_tag(timebase.to_tag()), timebase);
        }
        assert_eq!(TrackTimebase::from_tag(200), TrackTimebase::Musical);
        assert_eq!(TrackTimebase::Musical.toggled(), TrackTimebase::Linear);
        assert_eq!(TrackTimebase::Linear.toggled(), TrackTimebase::Musical);
    }
}

/// Project timebase: the ruler's ticks and every position readout.
mod time_display_ruler_tests {
    use super::*;

    fn state_at_zoom(ppb: f32) -> TimelineState {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.viewport.pixels_per_second = ppb / 0.5;
        state.sync_pixels_per_beat();
        state.update_viewport_size(1200.0, 500.0);
        state
    }

    /// Bars+Beats keeps the ruler on the arrangement grid, so the ruler and the
    /// grid behind the clips are literally the same lines.
    #[test]
    fn bars_and_beats_reuses_the_arrangement_grid() {
        let state = state_at_zoom(40.0);
        let ruler = state.ruler_grid_lines(1200.0);
        let grid = state.arrangement_grid_lines(1200.0);
        assert_eq!(ruler.len(), grid.len());
        assert!(ruler
            .iter()
            .zip(grid.iter())
            .all(|(a, b)| a.x == b.x && a.beat == b.beat));
    }

    /// A time-based timebase gets its own ticks. Every labelled one must land on
    /// a whole multiple of the chosen step in *seconds*, not on a bar.
    #[test]
    fn seconds_ruler_labels_land_on_round_clock_positions() {
        let mut state = state_at_zoom(40.0);
        state.time_display_format = TimeDisplayFormat::Seconds;
        let lines = state.ruler_grid_lines(1200.0);
        assert!(!lines.is_empty());

        let labelled: Vec<f64> = lines
            .iter()
            .filter(|line| line.show_label)
            .map(|line| state.seconds_at_beat(line.beat as f64))
            .collect();
        assert!(labelled.len() >= 2, "expected several labels");

        // Consecutive labels are one constant step apart, and that step is a
        // round number of seconds.
        let step = labelled[1] - labelled[0];
        assert!(step > 0.0);
        for pair in labelled.windows(2) {
            assert!(
                ((pair[1] - pair[0]) - step).abs() < 1.0e-3,
                "uneven label spacing: {pair:?}"
            );
        }
        for seconds in &labelled {
            let slots = seconds / step;
            assert!(
                (slots - slots.round()).abs() < 1.0e-3,
                "{seconds}s is not a whole step of {step}s"
            );
        }
    }

    /// Labels must never collide, at any zoom, in any time-based format.
    #[test]
    fn time_ruler_labels_never_overlap_at_any_zoom() {
        for format in [
            TimeDisplayFormat::Seconds,
            TimeDisplayFormat::Timecode,
            TimeDisplayFormat::Samples,
        ] {
            for ppb in [2.0, 12.0, 40.0, 160.0, 600.0] {
                let mut state = state_at_zoom(ppb);
                state.time_display_format = format;
                let lines = state.ruler_grid_lines(1200.0);
                let mut previous = f32::NEG_INFINITY;
                for line in lines.iter().filter(|line| line.show_label) {
                    assert!(
                        line.x - previous >= 60.0,
                        "{format:?} @ppb={ppb}: labels {previous} and {} too close",
                        line.x
                    );
                    previous = line.x;
                }
            }
        }
    }

    /// Every format must produce a readout, and switching format must change it
    /// — a setting that renders the same string everywhere would be a lie.
    #[test]
    fn each_timebase_formats_the_same_position_differently() {
        let mut state = state_at_zoom(40.0);
        state.project_sample_rate = 48_000;
        // Beat 8 at 120 BPM is 4 seconds.
        let mut seen = Vec::new();
        for format in TimeDisplayFormat::ALL {
            state.time_display_format = format;
            let text = state.format_position_at(8.0);
            assert!(!text.is_empty(), "{format:?} produced nothing");
            seen.push(text);
        }
        assert_eq!(seen[0], "3.1", "bars+beats");
        assert_eq!(seen[1], "0:04.000", "seconds");
        assert_eq!(seen[2], "00:00:04:00", "timecode");
        assert_eq!(seen[3], "192000", "samples");
    }

    /// The grid behind the clips follows the timebase. A timecode ruler over a
    /// bar/beat grid is the "the grid didn't follow" bug.
    #[test]
    fn the_arrangement_grid_follows_the_timebase() {
        let musical = state_at_zoom(40.0);
        let before: Vec<(f32, f32)> = musical
            .arrangement_grid_lines(1200.0)
            .iter()
            .map(|l| (l.x, l.beat))
            .collect();

        let mut timecode = state_at_zoom(40.0);
        timecode.time_display_format = TimeDisplayFormat::Timecode;
        let after: Vec<(f32, f32)> = timecode
            .arrangement_grid_lines(1200.0)
            .iter()
            .map(|l| (l.x, l.beat))
            .collect();

        assert_ne!(before, after, "grid did not follow the timebase");
        // And the ruler draws that same set, so a label always names the line
        // beneath it.
        let ruler: Vec<(f32, f32)> = timecode
            .ruler_grid_lines(1200.0)
            .iter()
            .map(|l| (l.x, l.beat))
            .collect();
        assert_eq!(ruler, after);
    }

    /// The cache is keyed by everything the generator reads, so flipping the
    /// timebase cannot keep serving the previous grid.
    #[test]
    fn the_grid_cache_is_invalidated_by_the_timebase() {
        let musical = state_at_zoom(40.0);
        let first = musical.arrangement_grid_lines(1200.0);
        let mut seconds = state_at_zoom(40.0);
        seconds.time_display_format = TimeDisplayFormat::Seconds;
        let second = seconds.arrangement_grid_lines(1200.0);
        assert_ne!(
            first.iter().map(|l| l.x).collect::<Vec<_>>(),
            second.iter().map(|l| l.x).collect::<Vec<_>>()
        );
    }

    /// Regression: the label used to be re-derived from the line's `f32` beat,
    /// so 0.5 s came back as 0.49999997 and truncated to frame 14 instead of 15.
    /// Every timecode label must sit on a whole frame.
    #[test]
    fn timecode_labels_land_on_whole_frames() {
        let mut state = state_at_zoom(40.0);
        state.bpm = 140.0;
        state.sync_pixels_per_beat();
        state.time_display_format = TimeDisplayFormat::Timecode;
        state.timecode_rate = TimecodeRate::Fps30;

        let lines = state.ruler_grid_lines(1200.0);
        let labels: Vec<String> = lines
            .iter()
            .filter(|line| line.show_label)
            .map(|line| state.format_grid_line_label(line))
            .collect();
        assert!(labels.len() >= 3, "expected several labels");
        for label in &labels {
            let frames: u32 = label
                .rsplit(':')
                .next()
                .expect("frame field")
                .parse()
                .expect("numeric frames");
            assert!(frames < 30, "{label} has an out-of-range frame");
        }
        // The step is half a second here, so the frame field must alternate
        // exactly 00 / 15 — never 14 or 29.
        let frame_fields: Vec<&str> = labels
            .iter()
            .map(|l| l.rsplit(':').next().unwrap())
            .collect();
        assert!(
            frame_fields.iter().all(|f| *f == "00" || *f == "15"),
            "frames drifted off whole steps: {frame_fields:?}"
        );
    }

    /// Snapping follows the drawn grid, so a clip lands on a line the user can
    /// see rather than on a note value nothing is drawn at.
    #[test]
    fn snapping_follows_the_time_grid() {
        let mut state = state_at_zoom(40.0);
        state.time_display_format = TimeDisplayFormat::Seconds;
        state.snap_to_grid = true;

        let step_seconds = state.time_grid_step().minor;
        let snap = SnapSettings::from_timeline(&state).to_musical();
        let step_beats = snap.step_beats().expect("a time grid step");
        // The snap step is the grid's minor step, expressed in beats.
        let expected = step_seconds * state.bpm as f64 / 60.0;
        assert!(
            (step_beats - expected).abs() < 1.0e-6,
            "snap step {step_beats} != grid step {expected}"
        );

        // And a snapped position is a whole number of grid steps in seconds.
        let snapped = super::super::musical_snap::snap_beat(3.317, snap, false);
        let slots = state.seconds_at_beat(snapped) / step_seconds;
        assert!(
            (slots - slots.round()).abs() < 1.0e-4,
            "snapped to {snapped} beats, which is {slots} grid steps"
        );
    }

    /// The resolution chip must report the step that actually applies, not a
    /// note value nothing snaps to.
    #[test]
    fn the_grid_chip_reports_the_clock_step_when_time_based() {
        let mut state = state_at_zoom(40.0);
        let musical_label = state.grid_step_label();
        state.time_display_format = TimeDisplayFormat::Timecode;
        let timecode_label = state.grid_step_label();
        assert_ne!(musical_label, timecode_label);
        assert!(
            timecode_label.ends_with(" f")
                || timecode_label.ends_with(" s")
                || timecode_label.ends_with(" m"),
            "unexpected chip label {timecode_label}"
        );
    }
}
