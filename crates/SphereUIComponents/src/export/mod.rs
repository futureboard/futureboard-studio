//! Arrangement export UI: the settings model, the Export dialog that chooses
//! what to render, and the Render dialog that runs it and reports on every
//! file. Rendering/encoding lives in the engine + SphereEncoder; this layer
//! collects settings, builds a plain job + snapshot, and drives a cancellable
//! background render without holding any GPUI borrow during the work.

mod export_settings;
mod export_window;
mod render_dialog;
mod ui_kit;

pub use export_settings::{
    ExportChannelMode, ExportChannelPreset, ExportEstimate, ExportNormalizeChoice,
    ExportProjectDefaults, ExportRangeChoice, ExportRenderMode, ExportSampleRateChoice,
    ExportSettings, ExportSettingsError, ExportTailChoice, ExportTrackTarget,
};
pub use export_window::{
    open_export_arrangement_window, ExportArrangementWindow, ExportIntent, EXPORT_WINDOW_WIDTH,
};
pub use render_dialog::{RealtimeTransportHooks, RenderDialog};

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_encoder::{AudioFileFormat, AudioSampleFormat};
    use DirectAudio::types::{EngineProjectSnapshot, EngineRoutingSnapshot, EngineTrackSnapshot};

    fn snapshot_with_content(end_beat: f64) -> EngineProjectSnapshot {
        use DirectAudio::types::EngineClipSnapshot;
        let clips = if end_beat > 0.0 {
            vec![EngineClipSnapshot {
                id: "clip-1".to_string(),
                track_id: "track-1".to_string(),
                asset_id: "asset-1".to_string(),
                media_path: None,
                start_beat: 0.0,
                duration_beats: end_beat,
                offset_seconds: 0.0,
                gain: 1.0,
                muted: false,
                ara_rendered: false,
                fades: None,
                stretch: SphereAudioProcessor::StretchParams::default(),
                audio_process: None,
            }]
        } else {
            Vec::new()
        };
        EngineProjectSnapshot {
            spatial: Default::default(),
            project_id: "p".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: 48_000,
            tracks: vec![EngineTrackSnapshot {
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
            }],
            clips,
            midi_clips: Vec::new(),
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: 48_000,
                buffer_size: 512,
            },
        }
    }

    fn defaults() -> ExportProjectDefaults {
        ExportProjectDefaults {
            project_sample_rate: 48_000,
            master_volume: 1.0,
            content_end_beat: 4.0,
            time_selection: None,
            loop_range: None,
            mp3_available: false,
            track_targets: Vec::new(),
            live_sample_rate: 48_000,
        }
    }

    fn target(id: &str, name: &str, source: bool) -> ExportTrackTarget {
        ExportTrackTarget {
            id: id.to_string(),
            name: name.to_string(),
            include_in_multitrack: source,
            kind_label: if source { "Audio" } else { "Bus" }.to_string(),
        }
    }

    fn valid_wav() -> ExportSettings {
        let mut s = ExportSettings::default();
        s.output_path = Some(std::env::temp_dir().join("fb-export-test.wav"));
        s
    }

    #[test]
    fn valid_wav_settings_pass() {
        assert!(valid_wav().validate(&defaults()).is_ok());
    }

    #[test]
    fn a_render_needs_the_mixdown_or_a_channel() {
        let mut settings = valid_wav();
        settings.include_mixdown = false;
        assert_eq!(
            settings.validate(&defaults()),
            Err(ExportSettingsError::NothingToRender)
        );
        let mut d = defaults();
        d.track_targets = vec![target("track-1", "Vox", true)];
        settings.toggle_track("track-1");
        assert!(settings.validate(&d).is_ok());
    }

    /// One job carries the mixdown and the chosen channels together; the
    /// channels land numbered in mixer order in `<base> Stems/`, never
    /// normalized.
    #[test]
    fn one_job_carries_the_mixdown_and_the_chosen_channels() {
        let mut snapshot = snapshot_with_content(4.0);
        let mut bus = snapshot.tracks[0].clone();
        bus.id = "bus-1".to_string();
        bus.track_type = "bus".to_string();
        snapshot.tracks[0].output_track_id = Some(bus.id.clone());
        snapshot.tracks.push(bus);
        let mut defaults = defaults();
        defaults.track_targets = vec![
            target("track-1", "Lead / Vox", true),
            target("bus-1", "Drum Bus", false),
        ];
        let mut settings = valid_wav();
        settings.normalize = ExportNormalizeChoice::PeakDb(-1.0);
        settings.apply_channel_preset(ExportChannelPreset::All, &defaults);
        let job = settings.to_job(&snapshot, &defaults, "Song").unwrap();
        assert!(job.mixdown.is_some());
        assert_eq!(job.tracks.len(), 2);
        assert_eq!(job.tracks[0].track_id, "track-1");
        assert_eq!(job.tracks[1].track_id, "bus-1");
        assert!(job.tracks[0]
            .request
            .output_path
            .ends_with(std::path::Path::new("Song Stems").join("01 Lead _ Vox.wav")));
        assert!(job.tracks.iter().all(|t| matches!(
            t.request.render.normalize,
            DirectAudio::ExportNormalizeMode::None
        )));

        // Channels only: no mixdown in the job.
        settings.include_mixdown = false;
        settings.apply_channel_preset(ExportChannelPreset::SourceTracks, &defaults);
        let job = settings.to_job(&snapshot, &defaults, "Song").unwrap();
        assert!(job.mixdown.is_none());
        assert_eq!(job.tracks.len(), 1);
    }

    /// A realtime render records at the device rate and cannot normalize.
    #[test]
    fn realtime_takes_the_device_rate_and_refuses_normalization() {
        let mut d = defaults();
        d.live_sample_rate = 44_100;
        let mut settings = valid_wav();
        settings.render_mode = ExportRenderMode::Realtime;
        assert_eq!(settings.resolved_sample_rate(&d), 44_100);
        assert!(settings.validate(&d).is_ok());
        settings.normalize = ExportNormalizeChoice::PeakDb(-1.0);
        assert_eq!(
            settings.validate(&d),
            Err(ExportSettingsError::RealtimeNormalize)
        );
        d.live_sample_rate = 0;
        settings.normalize = ExportNormalizeChoice::Off;
        assert_eq!(
            settings.validate(&d),
            Err(ExportSettingsError::RealtimeUnavailable)
        );
    }

    #[test]
    fn missing_output_path_fails() {
        let s = ExportSettings::default();
        assert_eq!(
            s.validate(&defaults()),
            Err(ExportSettingsError::NoOutputPath)
        );
    }

    #[test]
    fn mp3_disabled_fails_cleanly() {
        let mut s = valid_wav();
        s.format = AudioFileFormat::Mp3;
        s.output_path = Some(std::env::temp_dir().join("fb-export-test.mp3"));
        assert_eq!(
            s.validate(&defaults()),
            Err(ExportSettingsError::Mp3Unavailable)
        );
    }

    #[test]
    fn mp3_enabled_passes_validation() {
        let mut d = defaults();
        d.mp3_available = true;
        let mut s = valid_wav();
        s.format = AudioFileFormat::Mp3;
        s.sample_rate = ExportSampleRateChoice::Hz48000;
        assert!(s.validate(&d).is_ok());
    }

    #[test]
    fn extension_follows_format_deterministically() {
        let mut s = valid_wav();
        s.format = AudioFileFormat::Flac;
        let path = s.normalized_output_path().unwrap();
        assert_eq!(path.extension().unwrap(), "flac");
    }

    #[test]
    fn entire_arrangement_resolves_from_content_bounds() {
        let snapshot = snapshot_with_content(4.0); // 4 beats @ 120bpm = 2.0s
        let req = valid_wav().to_request(&snapshot, &defaults()).unwrap();
        // 2 seconds at 48k = 96000 frames.
        assert_eq!(req.render.start_sample, 0);
        assert_eq!(req.render.end_sample, 96_000);
        assert_eq!(req.render.sample_rate, 48_000);
        assert_eq!(req.render.channels, 2);
    }

    #[test]
    fn empty_arrangement_reports_no_content() {
        let snapshot = snapshot_with_content(0.0);
        let result = valid_wav().to_request(&snapshot, &defaults());
        assert_eq!(result.err(), Some(ExportSettingsError::NoContent));
    }

    #[test]
    fn custom_range_converts_beats_to_samples() {
        let snapshot = snapshot_with_content(16.0);
        let mut s = valid_wav();
        // beats 4..8 @ 120bpm = 2.0s..4.0s = 96000..192000 frames @ 48k.
        s.range = ExportRangeChoice::Custom {
            start_beat: 4.0,
            end_beat: 8.0,
        };
        let req = s.to_request(&snapshot, &defaults()).unwrap();
        assert_eq!(req.render.start_sample, 96_000);
        assert_eq!(req.render.end_sample, 192_000);
    }

    #[test]
    fn inverted_custom_range_fails() {
        let snapshot = snapshot_with_content(16.0);
        let mut s = valid_wav();
        s.range = ExportRangeChoice::Custom {
            start_beat: 8.0,
            end_beat: 4.0,
        };
        assert_eq!(
            s.to_request(&snapshot, &defaults()).err(),
            Some(ExportSettingsError::InvalidRange)
        );
    }

    #[test]
    fn project_sample_rate_resolves() {
        let mut d = defaults();
        d.project_sample_rate = 44_100;
        let snapshot = snapshot_with_content(4.0);
        let mut s = valid_wav();
        s.sample_rate = ExportSampleRateChoice::Project;
        let req = s.to_request(&snapshot, &d).unwrap();
        assert_eq!(req.render.sample_rate, 44_100);
    }

    #[test]
    fn flac_bit_depth_maps_to_sample_format() {
        let snapshot = snapshot_with_content(4.0);
        let mut s = valid_wav();
        s.format = AudioFileFormat::Flac;
        s.flac_bit_depth = 16;
        let req = s.to_request(&snapshot, &defaults()).unwrap();
        assert_eq!(req.sample_format, AudioSampleFormat::I16);
    }

    /// The "Source tracks" pick leaves routing channels out; "All" takes them.
    #[test]
    fn channel_presets_pick_what_they_say() {
        let mut d = defaults();
        d.track_targets = vec![
            target("track-1", "Vox", true),
            target("bus-1", "Drum Bus", false),
        ];
        let mut settings = valid_wav();
        settings.apply_channel_preset(ExportChannelPreset::SourceTracks, &d);
        assert_eq!(settings.selected_tracks, vec!["track-1".to_string()]);
        settings.apply_channel_preset(ExportChannelPreset::All, &d);
        assert_eq!(settings.batch_target_count(&d), 2);
        assert_eq!(settings.file_count(&d), 3);
        settings.apply_channel_preset(ExportChannelPreset::None, &d);
        assert_eq!(settings.file_count(&d), 1);
    }

    /// The dialog's readouts must come from the same request the engine gets.
    #[test]
    fn estimate_matches_request_geometry() {
        let snapshot = snapshot_with_content(4.0);
        let settings = valid_wav();
        let d = defaults();
        let request = settings.to_request(&snapshot, &d).unwrap();
        let estimate = settings.estimate(&snapshot, &d).unwrap();
        assert_eq!(estimate.sample_rate, request.render.sample_rate);
        assert_eq!(estimate.channels, request.render.channels);
        assert_eq!(estimate.start_sample, request.render.start_sample);
        assert_eq!(estimate.end_sample, request.render.end_sample);
        assert_eq!(estimate.content_frames, request.render.content_frames());
        assert_eq!(estimate.max_tail_frames, request.render.max_tail_frames());
        assert_eq!(estimate.file_count, 1);
        // 2 s of content + the 5 s default tail, stereo 24-bit at 48 kHz.
        assert_eq!(estimate.content_frames, 96_000);
        assert_eq!(estimate.max_tail_frames, 240_000);
        let frames = estimate.content_frames + estimate.max_tail_frames;
        assert_eq!(estimate.uncompressed_bytes, Some(frames * 2 * 3 + 44));
    }

    /// The peak target and both "until silence" parameters are real engine
    /// inputs, so the dialog is allowed to name them.
    #[test]
    fn peak_target_and_tail_values_reach_the_request() {
        let snapshot = snapshot_with_content(4.0);
        let mut s = valid_wav();
        s.normalize = ExportNormalizeChoice::PeakDb(-3.0);
        s.tail = ExportTailChoice::UntilSilence {
            max_seconds: 7.5,
            threshold_db: -48.0,
        };
        let req = s.to_request(&snapshot, &defaults()).unwrap();
        assert!(matches!(
            req.render.normalize,
            DirectAudio::ExportNormalizeMode::PeakDb(db) if (db + 3.0).abs() < 1e-6
        ));
        assert!(matches!(
            req.render.tail,
            DirectAudio::ExportTailMode::UntilSilence {
                max_seconds,
                threshold_db,
            } if (max_seconds - 7.5).abs() < 1e-9 && (threshold_db + 48.0).abs() < 1e-6
        ));
        // 7.5 s at 48 kHz is the cap the progress denominator uses.
        assert_eq!(req.render.max_tail_frames(), 360_000);
    }
}
