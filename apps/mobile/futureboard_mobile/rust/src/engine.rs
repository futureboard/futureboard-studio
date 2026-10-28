//! Core standalone DAW arrangement, mixer, MIDI sequencer, and DSP engine for Futureboard Studio Mobile.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use builtin_dsp_core::spectrum::{SpectrumAnalyzer, SPECTRUM_BINS};
use ebur128::{EbuR128, Mode as EbuMode};
use serde::{Deserialize, Serialize};
use solfege_spatialaudio::{BinauralSource, RoomPosition, RoomSettings, SourceParams};

use crate::audio_io::{self, StereoAudioBuffer};
use crate::plugin_host::{
    plugin_catalog, BuiltinPluginInstance, PluginCatalogEntry, PluginMeterSnapshot, PluginParamSpec,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrackKind {
    Audio,
    Instrument,
    DrumKit,
    Bus,
    Master,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MidiNote {
    pub id: String,
    pub pitch: u8,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub velocity: u8,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MidiClip {
    pub id: String,
    pub name: String,
    pub start_beat: f64,
    pub length_beats: f64,
    pub loop_length_beats: f64,
    pub loop_enabled: bool,
    pub color_hex: String,
    pub notes: Vec<MidiNote>,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioClip {
    pub id: String,
    pub name: String,
    pub source_id: String,
    pub source_path: String,
    pub start_beat: f64,
    pub length_beats: f64,
    pub offset_beats: f64,
    pub gain_db: f32,
    pub pitch_semitones: f32,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub color_hex: String,
    pub waveform_peaks: Vec<f32>,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationPoint {
    pub beat: f64,
    pub value: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationLane {
    pub id: String,
    pub target_param: String,
    pub enabled: bool,
    pub points: Vec<AutomationPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendSlot {
    pub target_bus_id: String,
    pub level_db: f32,
    pub enabled: bool,
    pub pre_fader: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertSlotSnapshot {
    pub slot_index: usize,
    pub plugin_id: String,
    pub name: String,
    pub enabled: bool,
    pub active_preset: String,
    pub params: Vec<PluginParamSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackSnapshot {
    pub id: String,
    pub name: String,
    pub kind: TrackKind,
    pub color_hex: String,
    pub volume_db: f32,
    pub pan: f32,
    pub muted: bool,
    pub soloed: bool,
    pub armed: bool,
    pub input_monitoring: bool,
    pub automation_read: bool,
    pub spatial_x: f32,
    pub spatial_y: f32,
    pub spatial_z: f32,
    pub spatial_binaural: bool,
    pub instrument: Option<InsertSlotSnapshot>,
    pub inserts: Vec<InsertSlotSnapshot>,
    pub sends: Vec<SendSlot>,
    pub midi_clips: Vec<MidiClip>,
    pub audio_clips: Vec<AudioClip>,
    pub automation_lanes: Vec<AutomationLane>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSnapshot {
    pub title: String,
    pub bpm: f64,
    pub time_sig_num: u32,
    pub time_sig_den: u32,
    pub sample_rate: u32,
    pub playhead_beats: f64,
    pub is_playing: bool,
    pub is_recording: bool,
    pub loop_enabled: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
    pub metronome_enabled: bool,
    pub count_in_enabled: bool,
    pub master_binaural_enabled: bool,
    pub room_reverb_mix: f32,
    pub selected_track_id: String,
    pub selected_clip_id: Option<String>,
    pub can_undo: bool,
    pub can_redo: bool,
    pub tracks: Vec<TrackSnapshot>,
    pub master_track: TrackSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackMeterFrame {
    pub track_id: String,
    pub peak_l_db: f32,
    pub peak_r_db: f32,
    pub rms_l_db: f32,
    pub rms_r_db: f32,
    pub gain_reduction_db: f32,
    pub clipping: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySnapshot {
    pub playhead_beats: f64,
    pub is_playing: bool,
    pub is_recording: bool,
    pub bpm: f64,
    pub master_peak_l_db: f32,
    pub master_peak_r_db: f32,
    pub master_rms_l_db: f32,
    pub master_rms_r_db: f32,
    pub lufs_momentary: f32,
    pub lufs_short_term: f32,
    pub lufs_integrated: f32,
    pub true_peak_dbtp: f32,
    pub cpu_load_pct: f32,
    pub spectrum_db: Vec<f32>,
    pub track_meters: Vec<TrackMeterFrame>,
    pub inspected_plugin_meter: Option<PluginMeterSnapshot>,
}

pub struct TrackRuntime {
    pub snapshot: TrackSnapshot,
    pub instrument_instance: Option<BuiltinPluginInstance>,
    pub insert_instances: Vec<BuiltinPluginInstance>,
    pub binaural_source: BinauralSource,
    pub active_midi_notes: [bool; 128],
    pub buf_l: Vec<f32>,
    pub buf_r: Vec<f32>,
    pub peak_l: f32,
    pub peak_r: f32,
    pub rms_l: f32,
    pub rms_r: f32,
    pub gr_db: f32,
}

impl TrackRuntime {
    pub fn new(snapshot: TrackSnapshot, sample_rate: u32) -> Self {
        let mut rt = Self {
            snapshot,
            instrument_instance: None,
            insert_instances: Vec::new(),
            binaural_source: BinauralSource::new(sample_rate, 4096),
            active_midi_notes: [false; 128],
            buf_l: vec![0.0; 4096],
            buf_r: vec![0.0; 4096],
            peak_l: 0.0,
            peak_r: 0.0,
            rms_l: 0.0,
            rms_r: 0.0,
            gr_db: 0.0,
        };
        rt.rebuild_plugins_from_snapshot(sample_rate as f32);
        rt
    }

    pub fn rebuild_plugins_from_snapshot(&mut self, sample_rate: f32) {
        self.instrument_instance = self.snapshot.instrument.as_ref().and_then(|slot| {
            let mut inst = BuiltinPluginInstance::new(&slot.plugin_id, sample_rate)?;
            inst.enabled = slot.enabled;
            inst.active_preset = slot.active_preset.clone();
            for p in &slot.params {
                inst.set_param(&p.id, p.value);
            }
            Some(inst)
        });

        self.insert_instances.clear();
        for slot in &self.snapshot.inserts {
            if let Some(mut inst) = BuiltinPluginInstance::new(&slot.plugin_id, sample_rate) {
                inst.enabled = slot.enabled;
                inst.active_preset = slot.active_preset.clone();
                for p in &slot.params {
                    inst.set_param(&p.id, p.value);
                }
                self.insert_instances.push(inst);
            }
        }
        self.sync_snapshot_from_plugins();
    }

    pub fn sync_snapshot_from_plugins(&mut self) {
        self.snapshot.instrument = self.instrument_instance.as_ref().map(|inst| InsertSlotSnapshot {
            slot_index: 0,
            plugin_id: inst.plugin_id.clone(),
            name: inst.name.clone(),
            enabled: inst.enabled,
            active_preset: inst.active_preset.clone(),
            params: inst.param_specs.clone(),
        });

        self.snapshot.inserts = self
            .insert_instances
            .iter()
            .enumerate()
            .map(|(idx, inst)| InsertSlotSnapshot {
                slot_index: idx,
                plugin_id: inst.plugin_id.clone(),
                name: inst.name.clone(),
                enabled: inst.enabled,
                active_preset: inst.active_preset.clone(),
                params: inst.param_specs.clone(),
            })
            .collect();
    }

    pub fn all_notes_off(&mut self) {
        if let Some(inst) = &mut self.instrument_instance {
            inst.all_notes_off();
        }
        self.active_midi_notes.fill(false);
    }
}

#[derive(Debug, Clone)]
struct LiveMidiEvent {
    track_id: String,
    note: u8,
    velocity: u8,
    is_note_on: bool,
}

pub struct StudioMobileEngine {
    pub title: String,
    pub bpm: f64,
    pub time_sig_num: u32,
    pub time_sig_den: u32,
    pub sample_rate: u32,
    pub playhead_beats: f64,
    pub is_playing: bool,
    pub is_recording: bool,
    pub loop_enabled: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
    pub metronome_enabled: bool,
    pub count_in_enabled: bool,
    pub master_binaural_enabled: bool,
    pub room_settings: RoomSettings,
    pub selected_track_id: String,
    pub selected_clip_id: Option<String>,
    pub inspected_plugin_target: Option<(String, Option<usize>)>, // (track_id, None = instrument, Some(i) = insert i)

    pub tracks: Vec<TrackRuntime>,
    pub master_track: TrackRuntime,
    pub audio_pool: HashMap<String, Arc<StereoAudioBuffer>>,

    live_midi_queue: Vec<LiveMidiEvent>,
    recording_buffer_l: Vec<f32>,
    recording_buffer_r: Vec<f32>,
    recording_start_beat: f64,
    metronome_phase: f32,
    metronome_env: f32,
    metronome_freq: f32,

    spectrum: SpectrumAnalyzer,
    spectrum_bins_cache: [f32; SPECTRUM_BINS],
    ebur128: Option<EbuR128>,
    interleaved_meter_scratch: Vec<f32>,
    last_cpu_load_pct: f32,

    undo_stack: Vec<ProjectSnapshot>,
    redo_stack: Vec<ProjectSnapshot>,
    next_id_counter: u64,
}

impl StudioMobileEngine {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.clamp(8_000, 192_000);
        let master_snap = TrackSnapshot {
            id: "master".to_string(),
            name: "Master Bus".to_string(),
            kind: TrackKind::Master,
            color_hex: "#4C8DFF".to_string(),
            volume_db: 0.0,
            pan: 0.0,
            muted: false,
            soloed: false,
            armed: false,
            input_monitoring: false,
            automation_read: true,
            spatial_x: 0.0,
            spatial_y: 1.0,
            spatial_z: 0.0,
            spatial_binaural: false,
            instrument: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            midi_clips: Vec::new(),
            audio_clips: Vec::new(),
            automation_lanes: Vec::new(),
        };

        let ebur = EbuR128::new(
            2,
            sr,
            EbuMode::M | EbuMode::S | EbuMode::I | EbuMode::TRUE_PEAK,
        )
        .ok();

        let mut engine = Self {
            title: "Futureboard Mobile Session".to_string(),
            bpm: 124.0,
            time_sig_num: 4,
            time_sig_den: 4,
            sample_rate: sr,
            playhead_beats: 0.0,
            is_playing: false,
            is_recording: false,
            loop_enabled: true,
            loop_start_beats: 0.0,
            loop_end_beats: 16.0,
            metronome_enabled: false,
            count_in_enabled: false,
            master_binaural_enabled: false,
            room_settings: RoomSettings::default(),
            selected_track_id: "track-1".to_string(),
            selected_clip_id: None,
            inspected_plugin_target: None,
            tracks: Vec::new(),
            master_track: TrackRuntime::new(master_snap, sr),
            audio_pool: HashMap::new(),
            live_midi_queue: Vec::new(),
            recording_buffer_l: Vec::new(),
            recording_buffer_r: Vec::new(),
            recording_start_beat: 0.0,
            metronome_phase: 0.0,
            metronome_env: 0.0,
            metronome_freq: 1000.0,
            spectrum: SpectrumAnalyzer::new(sr as f32),
            spectrum_bins_cache: [-96.0; SPECTRUM_BINS],
            ebur128: ebur,
            interleaved_meter_scratch: vec![0.0; 8192],
            last_cpu_load_pct: 4.2,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            next_id_counter: 100,
        };
        engine.load_demo_project();
        engine
    }

    fn alloc_id(&mut self, prefix: &str) -> String {
        self.next_id_counter += 1;
        format!("{prefix}-{}", self.next_id_counter)
    }

    pub fn push_undo(&mut self) {
        let snap = self.snapshot();
        if self.undo_stack.len() >= 32 {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(snap);
        self.redo_stack.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.redo_stack.push(current);
        self.restore_from_snapshot(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo_stack.pop() else {
            return false;
        };
        let current = self.snapshot();
        self.undo_stack.push(current);
        self.restore_from_snapshot(next);
        true
    }

    pub fn snapshot(&mut self) -> ProjectSnapshot {
        for t in &mut self.tracks {
            t.sync_snapshot_from_plugins();
        }
        self.master_track.sync_snapshot_from_plugins();

        ProjectSnapshot {
            title: self.title.clone(),
            bpm: self.bpm,
            time_sig_num: self.time_sig_num,
            time_sig_den: self.time_sig_den,
            sample_rate: self.sample_rate,
            playhead_beats: self.playhead_beats,
            is_playing: self.is_playing,
            is_recording: self.is_recording,
            loop_enabled: self.loop_enabled,
            loop_start_beats: self.loop_start_beats,
            loop_end_beats: self.loop_end_beats,
            metronome_enabled: self.metronome_enabled,
            count_in_enabled: self.count_in_enabled,
            master_binaural_enabled: self.master_binaural_enabled,
            room_reverb_mix: self.room_settings.reflections,
            selected_track_id: self.selected_track_id.clone(),
            selected_clip_id: self.selected_clip_id.clone(),
            can_undo: !self.undo_stack.is_empty(),
            can_redo: !self.redo_stack.is_empty(),
            tracks: self.tracks.iter().map(|t| t.snapshot.clone()).collect(),
            master_track: self.master_track.snapshot.clone(),
        }
    }

    pub fn restore_from_snapshot(&mut self, snap: ProjectSnapshot) {
        self.stop_all_notes();
        self.title = snap.title;
        self.bpm = snap.bpm.clamp(20.0, 300.0);
        self.time_sig_num = snap.time_sig_num.clamp(1, 16);
        self.time_sig_den = snap.time_sig_den.clamp(1, 16);
        self.playhead_beats = snap.playhead_beats.max(0.0);
        self.loop_enabled = snap.loop_enabled;
        self.loop_start_beats = snap.loop_start_beats.max(0.0);
        self.loop_end_beats = snap.loop_end_beats.max(self.loop_start_beats + 1.0);
        self.metronome_enabled = snap.metronome_enabled;
        self.count_in_enabled = snap.count_in_enabled;
        self.master_binaural_enabled = snap.master_binaural_enabled;
        self.room_settings.reflections = snap.room_reverb_mix.clamp(0.0, 1.0);
        self.selected_track_id = snap.selected_track_id;
        self.selected_clip_id = snap.selected_clip_id;

        let sr = self.sample_rate;
        self.tracks = snap
            .tracks
            .into_iter()
            .map(|ts| TrackRuntime::new(ts, sr))
            .collect();
        self.master_track = TrackRuntime::new(snap.master_track, sr);
    }

    pub fn save_project_json(&mut self) -> String {
        let snap = self.snapshot();
        serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn load_project_json(&mut self, json_str: &str) -> Result<(), String> {
        let snap: ProjectSnapshot =
            serde_json::from_str(json_str).map_err(|e| format!("Invalid project JSON: {e}"))?;
        self.push_undo();
        self.restore_from_snapshot(snap);
        Ok(())
    }

    pub fn catalog(&self) -> Vec<PluginCatalogEntry> {
        plugin_catalog()
    }

    /// Populates a complete, musically rich 6-track + Master studio session using real Rust DSP crates.
    pub fn load_demo_project(&mut self) {
        self.stop_all_notes();
        self.tracks.clear();
        self.audio_pool.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();

        self.title = "Neon Horizon — Futureboard Demo".to_string();
        self.bpm = 124.0;
        self.time_sig_num = 4;
        self.time_sig_den = 4;
        self.playhead_beats = 0.0;
        self.loop_enabled = true;
        self.loop_start_beats = 0.0;
        self.loop_end_beats = 16.0;

        let sr = self.sample_rate;

        // Synthesize a studio vocal/harmonic audio stem for Track 4
        let vocal_stem = Arc::new(audio_io::synthesize_demo_vocal_stem(
            sr,
            self.bpm,
            16.0,
        ));
        let vocal_peaks =
            audio_io::compute_stereo_peaks(&vocal_stem.left, &vocal_stem.right, 128);
        self.audio_pool
            .insert("stem-vocal-1".to_string(), vocal_stem);

        // ── Track 1: Drum Kit (DrumSampler + Transient + Compresser) ─────────
        let mut drum_notes = Vec::new();
        for bar in 0..4 {
            let b0 = (bar * 4) as f64;
            // Four-on-the-floor kick (36) + syncopated ghost kick
            for beat in 0..4 {
                drum_notes.push(MidiNote {
                    id: format!("dk-{bar}-{beat}"),
                    pitch: 36,
                    start_beat: b0 + beat as f64,
                    duration_beats: 0.45,
                    velocity: 118,
                    muted: false,
                });
            }
            drum_notes.push(MidiNote {
                id: format!("dk-ghost-{bar}"),
                pitch: 36,
                start_beat: b0 + 2.75,
                duration_beats: 0.25,
                velocity: 88,
                muted: false,
            });
            // Snare (37) + Clap (40) on 2 and 4
            for snare_beat in [1.0, 3.0] {
                drum_notes.push(MidiNote {
                    id: format!("ds-{bar}-{snare_beat}"),
                    pitch: 37,
                    start_beat: b0 + snare_beat,
                    duration_beats: 0.4,
                    velocity: 112,
                    muted: false,
                });
                drum_notes.push(MidiNote {
                    id: format!("dc-{bar}-{snare_beat}"),
                    pitch: 40,
                    start_beat: b0 + snare_beat,
                    duration_beats: 0.4,
                    velocity: 96,
                    muted: false,
                });
            }
            // 16th closed hi-hats (38) + offbeat open hat (39)
            for step in 0..16 {
                let is_offbeat = step % 4 == 2;
                drum_notes.push(MidiNote {
                    id: format!("dh-{bar}-{step}"),
                    pitch: if is_offbeat { 39 } else { 38 },
                    start_beat: b0 + step as f64 * 0.25,
                    duration_beats: 0.2,
                    velocity: if is_offbeat {
                        102
                    } else if step % 2 == 0 {
                        84
                    } else {
                        62
                    },
                    muted: false,
                });
            }
        }

        let mut t1 = TrackRuntime::new(
            TrackSnapshot {
                id: "track-1".to_string(),
                name: "MPC Drum Kit".to_string(),
                kind: TrackKind::DrumKit,
                color_hex: "#E68A3E".to_string(),
                volume_db: -1.5,
                pan: 0.0,
                muted: false,
                soloed: false,
                armed: false,
                input_monitoring: false,
                automation_read: true,
                spatial_x: 0.0,
                spatial_y: 0.85,
                spatial_z: 0.0,
                spatial_binaural: false,
                instrument: None,
                inserts: Vec::new(),
                sends: vec![SendSlot {
                    target_bus_id: "bus-1".to_string(),
                    level_db: -12.0,
                    enabled: true,
                    pre_fader: false,
                }],
                midi_clips: vec![MidiClip {
                    id: "clip-drums-1".to_string(),
                    name: "Main Groove 16B".to_string(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    loop_length_beats: 16.0,
                    loop_enabled: true,
                    color_hex: "#E68A3E".to_string(),
                    notes: drum_notes,
                    muted: false,
                }],
                audio_clips: Vec::new(),
                automation_lanes: Vec::new(),
            },
            sr,
        );
        t1.instrument_instance = BuiltinPluginInstance::new("drumsampler", sr as f32);
        if let Some(tr) = BuiltinPluginInstance::new("transient", sr as f32) {
            t1.insert_instances.push(tr);
        }
        if let Some(cp) = BuiltinPluginInstance::new("compresser", sr as f32) {
            t1.insert_instances.push(cp);
        }
        t1.sync_snapshot_from_plugins();

        // ── Track 2: Sub & Pluck Bass (WrapSynth + C-1073 + FA-76) ───────────
        let bass_pattern: &[(u8, f64, f64, u8)] = &[
            // Bar 1: A1 (45)
            (45, 0.0, 0.75, 115),
            (45, 0.75, 0.75, 108),
            (45, 1.5, 0.5, 102),
            (57, 2.0, 0.5, 110),
            (45, 2.5, 0.75, 112),
            (52, 3.25, 0.75, 106),
            // Bar 2: F1 (41)
            (41, 4.0, 0.75, 115),
            (41, 4.75, 0.75, 108),
            (41, 5.5, 0.5, 104),
            (53, 6.0, 0.5, 110),
            (41, 6.5, 0.75, 112),
            (48, 7.25, 0.75, 106),
            // Bar 3: C2 (48)
            (48, 8.0, 0.75, 115),
            (48, 8.75, 0.75, 108),
            (48, 9.5, 0.5, 104),
            (60, 10.0, 0.5, 110),
            (48, 10.5, 0.75, 112),
            (55, 11.25, 0.75, 106),
            // Bar 4: G1 (43)
            (43, 12.0, 0.75, 115),
            (43, 12.75, 0.75, 108),
            (43, 13.5, 0.5, 104),
            (55, 14.0, 0.5, 112),
            (43, 14.5, 0.75, 114),
            (47, 15.25, 0.75, 110),
        ];
        let bass_notes = bass_pattern
            .iter()
            .enumerate()
            .map(|(idx, (p, s, d, v))| MidiNote {
                id: format!("bn-{idx}"),
                pitch: *p,
                start_beat: *s,
                duration_beats: *d,
                velocity: *v,
                muted: false,
            })
            .collect();

        let mut t2 = TrackRuntime::new(
            TrackSnapshot {
                id: "track-2".to_string(),
                name: "WrapSynth Bass".to_string(),
                kind: TrackKind::Instrument,
                color_hex: "#4C8DFF".to_string(),
                volume_db: -2.5,
                pan: -0.05,
                muted: false,
                soloed: false,
                armed: false,
                input_monitoring: false,
                automation_read: true,
                spatial_x: -0.1,
                spatial_y: 0.9,
                spatial_z: 0.0,
                spatial_binaural: false,
                instrument: None,
                inserts: Vec::new(),
                sends: Vec::new(),
                midi_clips: vec![MidiClip {
                    id: "clip-bass-1".to_string(),
                    name: "Driving Subline".to_string(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    loop_length_beats: 16.0,
                    loop_enabled: true,
                    color_hex: "#4C8DFF".to_string(),
                    notes: bass_notes,
                    muted: false,
                }],
                audio_clips: Vec::new(),
                automation_lanes: vec![AutomationLane {
                    id: "auto-bass-vol".to_string(),
                    target_param: "volume_db".to_string(),
                    enabled: false,
                    points: vec![
                        AutomationPoint {
                            beat: 0.0,
                            value: -3.0,
                        },
                        AutomationPoint {
                            beat: 8.0,
                            value: -1.5,
                        },
                        AutomationPoint {
                            beat: 16.0,
                            value: -2.5,
                        },
                    ],
                }],
            },
            sr,
        );
        if let Some(mut ws) = BuiltinPluginInstance::new("wrapsynth", sr as f32) {
            ws.apply_preset("Deep Sub Pluck Bass");
            t2.instrument_instance = Some(ws);
        }
        if let Some(c1073) = BuiltinPluginInstance::new("c1073", sr as f32) {
            t2.insert_instances.push(c1073);
        }
        if let Some(fa76) = BuiltinPluginInstance::new("fa76", sr as f32) {
            t2.insert_instances.push(fa76);
        }
        t2.sync_snapshot_from_plugins();

        // ── Track 3: Lead Synth (MeowSyn + EchoSpace + VerbSpace) ────────────
        let lead_melody: &[(u8, f64, f64, u8)] = &[
            (69, 0.0, 1.5, 104),
            (72, 1.5, 1.0, 98),
            (76, 2.5, 1.5, 108),
            (77, 4.0, 1.5, 104),
            (76, 5.5, 1.0, 96),
            (72, 6.5, 1.5, 100),
            (72, 8.0, 1.5, 106),
            (76, 9.5, 1.0, 102),
            (79, 10.5, 1.5, 112),
            (74, 12.0, 1.5, 106),
            (71, 13.5, 1.0, 98),
            (69, 14.5, 1.5, 104),
        ];
        let lead_notes = lead_melody
            .iter()
            .enumerate()
            .map(|(idx, (p, s, d, v))| MidiNote {
                id: format!("ln-{idx}"),
                pitch: *p,
                start_beat: *s,
                duration_beats: *d,
                velocity: *v,
                muted: false,
            })
            .collect();

        let mut t3 = TrackRuntime::new(
            TrackSnapshot {
                id: "track-3".to_string(),
                name: "MeowSyn Lead".to_string(),
                kind: TrackKind::Instrument,
                color_hex: "#9B6BFF".to_string(),
                volume_db: -4.0,
                pan: 0.18,
                muted: false,
                soloed: false,
                armed: false,
                input_monitoring: false,
                automation_read: true,
                spatial_x: 0.28,
                spatial_y: 0.75,
                spatial_z: 0.25,
                spatial_binaural: false,
                instrument: None,
                inserts: Vec::new(),
                sends: vec![SendSlot {
                    target_bus_id: "bus-1".to_string(),
                    level_db: -6.0,
                    enabled: true,
                    pre_fader: false,
                }],
                midi_clips: vec![MidiClip {
                    id: "clip-lead-1".to_string(),
                    name: "Horizon Hook".to_string(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    loop_length_beats: 16.0,
                    loop_enabled: true,
                    color_hex: "#9B6BFF".to_string(),
                    notes: lead_notes,
                    muted: false,
                }],
                audio_clips: Vec::new(),
                automation_lanes: Vec::new(),
            },
            sr,
        );
        t3.instrument_instance = BuiltinPluginInstance::new("meowsyn", sr as f32);
        if let Some(echo) = BuiltinPluginInstance::new("echospace", sr as f32) {
            t3.insert_instances.push(echo);
        }
        t3.sync_snapshot_from_plugins();

        // ── Track 4: Studio Vocal / Harmonic Audio Stem (Audio + EQ-Z8 + FA-2A)
        let mut t4 = TrackRuntime::new(
            TrackSnapshot {
                id: "track-4".to_string(),
                name: "Atmos Vocal Stem".to_string(),
                kind: TrackKind::Audio,
                color_hex: "#38B87C".to_string(),
                volume_db: -3.5,
                pan: -0.14,
                muted: false,
                soloed: false,
                armed: false,
                input_monitoring: false,
                automation_read: true,
                spatial_x: -0.25,
                spatial_y: 0.8,
                spatial_z: 0.2,
                spatial_binaural: false,
                instrument: None,
                inserts: Vec::new(),
                sends: vec![SendSlot {
                    target_bus_id: "bus-1".to_string(),
                    level_db: -8.0,
                    enabled: true,
                    pre_fader: false,
                }],
                midi_clips: Vec::new(),
                audio_clips: vec![AudioClip {
                    id: "clip-audio-1".to_string(),
                    name: "Atmos Chords & Vocal Take.wav".to_string(),
                    source_id: "stem-vocal-1".to_string(),
                    source_path: "builtin://demo_vocal_stem.wav".to_string(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    offset_beats: 0.0,
                    gain_db: 0.0,
                    pitch_semitones: 0.0,
                    fade_in_beats: 0.5,
                    fade_out_beats: 1.0,
                    color_hex: "#38B87C".to_string(),
                    waveform_peaks: vocal_peaks,
                    muted: false,
                }],
                automation_lanes: Vec::new(),
            },
            sr,
        );
        if let Some(mut eq) = BuiltinPluginInstance::new("equz8", sr as f32) {
            eq.apply_preset("Vocal Presence & Air");
            t4.insert_instances.push(eq);
        }
        if let Some(fa2a) = BuiltinPluginInstance::new("fa2a", sr as f32) {
            t4.insert_instances.push(fa2a);
        }
        t4.sync_snapshot_from_plugins();

        // ── Track 5: SoundFont Poly Keys (SoundFont + Imager) ────────────────
        let chord_notes_data: &[(u8, f64, f64, u8)] = &[
            // Am7 (bar 1)
            (57, 0.0, 3.8, 86),
            (60, 0.0, 3.8, 84),
            (64, 0.0, 3.8, 82),
            (67, 0.0, 3.8, 80),
            // Fmaj7 (bar 2)
            (53, 4.0, 3.8, 86),
            (57, 4.0, 3.8, 84),
            (60, 4.0, 3.8, 82),
            (64, 4.0, 3.8, 80),
            // Cmaj7 (bar 3)
            (60, 8.0, 3.8, 88),
            (64, 8.0, 3.8, 84),
            (67, 8.0, 3.8, 82),
            (71, 8.0, 3.8, 80),
            // G6 (bar 4)
            (55, 12.0, 3.8, 88),
            (59, 12.0, 3.8, 84),
            (62, 12.0, 3.8, 82),
            (64, 12.0, 3.8, 80),
        ];
        let chord_notes = chord_notes_data
            .iter()
            .enumerate()
            .map(|(idx, (p, s, d, v))| MidiNote {
                id: format!("cn-{idx}"),
                pitch: *p,
                start_beat: *s,
                duration_beats: *d,
                velocity: *v,
                muted: false,
            })
            .collect();

        let mut t5 = TrackRuntime::new(
            TrackSnapshot {
                id: "track-5".to_string(),
                name: "SoundFont Keys".to_string(),
                kind: TrackKind::Instrument,
                color_hex: "#2BB3C0".to_string(),
                volume_db: -5.0,
                pan: 0.12,
                muted: false,
                soloed: false,
                armed: false,
                input_monitoring: false,
                automation_read: true,
                spatial_x: 0.2,
                spatial_y: 0.65,
                spatial_z: 0.1,
                spatial_binaural: false,
                instrument: None,
                inserts: Vec::new(),
                sends: vec![SendSlot {
                    target_bus_id: "bus-1".to_string(),
                    level_db: -9.0,
                    enabled: true,
                    pre_fader: false,
                }],
                midi_clips: vec![MidiClip {
                    id: "clip-keys-1".to_string(),
                    name: "Warm Progression".to_string(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    loop_length_beats: 16.0,
                    loop_enabled: true,
                    color_hex: "#2BB3C0".to_string(),
                    notes: chord_notes,
                    muted: false,
                }],
                audio_clips: Vec::new(),
                automation_lanes: Vec::new(),
            },
            sr,
        );
        t5.instrument_instance = BuiltinPluginInstance::new("soundfont", sr as f32);
        if let Some(im) = BuiltinPluginInstance::new("imager", sr as f32) {
            t5.insert_instances.push(im);
        }
        t5.sync_snapshot_from_plugins();

        // ── Bus 1: Space Return Bus (VerbSpace + EchoSpace) ──────────────────
        let mut bus1 = TrackRuntime::new(
            TrackSnapshot {
                id: "bus-1".to_string(),
                name: "VerbSpace Aux".to_string(),
                kind: TrackKind::Bus,
                color_hex: "#D95B82".to_string(),
                volume_db: -3.0,
                pan: 0.0,
                muted: false,
                soloed: false,
                armed: false,
                input_monitoring: false,
                automation_read: true,
                spatial_x: 0.0,
                spatial_y: 0.5,
                spatial_z: 0.4,
                spatial_binaural: false,
                instrument: None,
                inserts: Vec::new(),
                sends: Vec::new(),
                midi_clips: Vec::new(),
                audio_clips: Vec::new(),
                automation_lanes: Vec::new(),
            },
            sr,
        );
        if let Some(mut verb) = BuiltinPluginInstance::new("verbspace", sr as f32) {
            verb.set_param("mix_pct", 100.0);
            bus1.insert_instances.push(verb);
        }
        bus1.sync_snapshot_from_plugins();

        // ── Master Track: EQ-ZX + Z-Comp + BurnLimit ─────────────────────────
        self.master_track.insert_instances.clear();
        if let Some(mut eqzx) = BuiltinPluginInstance::new("equzx", sr as f32) {
            eqzx.apply_preset("Master Smile Contour");
            self.master_track.insert_instances.push(eqzx);
        }
        if let Some(zcomp) = BuiltinPluginInstance::new("zcomp", sr as f32) {
            self.master_track.insert_instances.push(zcomp);
        }
        if let Some(burn) = BuiltinPluginInstance::new("burnlimit", sr as f32) {
            self.master_track.insert_instances.push(burn);
        }
        self.master_track.sync_snapshot_from_plugins();

        self.tracks = vec![t1, t2, t3, t4, t5, bus1];
        self.selected_track_id = "track-1".to_string();
        self.selected_clip_id = Some("clip-drums-1".to_string());
        self.inspected_plugin_target = Some(("track-4".to_string(), Some(0)));
    }

    // ── Transport & Arrangement Mutations ────────────────────────────────────

    pub fn set_playing(&mut self, playing: bool) {
        if self.is_playing == playing {
            return;
        }
        self.is_playing = playing;
        if !playing {
            if self.is_recording {
                self.finish_recording_clip();
            }
            self.stop_all_notes();
        }
    }

    pub fn set_recording(&mut self, recording: bool) {
        if self.is_recording == recording {
            return;
        }
        if recording {
            self.recording_buffer_l.clear();
            self.recording_buffer_r.clear();
            self.recording_start_beat = self.playhead_beats;
            self.is_recording = true;
            if !self.is_playing {
                self.is_playing = true;
            }
        } else {
            self.finish_recording_clip();
        }
    }

    fn finish_recording_clip(&mut self) {
        self.is_recording = false;
        if self.recording_buffer_l.len() < 512 {
            self.recording_buffer_l.clear();
            self.recording_buffer_r.clear();
            return;
        }
        let left = std::mem::take(&mut self.recording_buffer_l);
        let right = std::mem::take(&mut self.recording_buffer_r);
        let frames = left.len().min(right.len());
        let duration_secs = frames as f64 / self.sample_rate as f64;
        let length_beats = (duration_secs * (self.bpm / 60.0)).max(0.5);

        let decoded = Arc::new(StereoAudioBuffer {
            sample_rate: self.sample_rate,
            frames,
            left,
            right,
        });
        let peaks = audio_io::compute_stereo_peaks(&decoded.left, &decoded.right, 128);
        let src_id = self.alloc_id("rec-src");
        let clip_id = self.alloc_id("clip-rec");
        self.audio_pool.insert(src_id.clone(), decoded);

        // Find armed audio track or first audio track
        let target_idx = self
            .tracks
            .iter()
            .position(|t| t.snapshot.kind == TrackKind::Audio && t.snapshot.armed)
            .or_else(|| {
                self.tracks
                    .iter()
                    .position(|t| t.snapshot.kind == TrackKind::Audio)
            });

        if let Some(idx) = target_idx {
            let color = self.tracks[idx].snapshot.color_hex.clone();
            self.tracks[idx].snapshot.audio_clips.push(AudioClip {
                id: clip_id.clone(),
                name: "Recorded Audio Take.wav".to_string(),
                source_id: src_id,
                source_path: "recorded://mic_take.wav".to_string(),
                start_beat: self.recording_start_beat,
                length_beats,
                offset_beats: 0.0,
                gain_db: 0.0,
                pitch_semitones: 0.0,
                fade_in_beats: 0.05,
                fade_out_beats: 0.05,
                color_hex: color,
                waveform_peaks: peaks,
                muted: false,
            });
            self.selected_clip_id = Some(clip_id);
        }
    }

    pub fn push_mic_input_interleaved(&mut self, interleaved_stereo_or_mono: &[f32], channels: usize) {
        if !self.is_recording || interleaved_stereo_or_mono.is_empty() {
            return;
        }
        let ch = channels.max(1);
        let frames = interleaved_stereo_or_mono.len() / ch;
        for f in 0..frames {
            let l = interleaved_stereo_or_mono[f * ch];
            let r = if ch > 1 {
                interleaved_stereo_or_mono[f * ch + 1]
            } else {
                l
            };
            self.recording_buffer_l.push(l);
            self.recording_buffer_r.push(r);
        }
    }

    pub fn seek_beats(&mut self, beats: f64) {
        self.playhead_beats = beats.max(0.0);
        self.stop_all_notes();
    }

    pub fn set_bpm(&mut self, bpm: f64) {
        self.bpm = bpm.clamp(20.0, 300.0);
        for t in &mut self.tracks {
            if let Some(inst) = &mut t.instrument_instance {
                inst.set_tempo_bpm(self.bpm as f32);
            }
        }
    }

    pub fn set_loop(&mut self, enabled: bool, start_beats: f64, end_beats: f64) {
        self.loop_enabled = enabled;
        self.loop_start_beats = start_beats.max(0.0);
        self.loop_end_beats = end_beats.max(self.loop_start_beats + 0.5);
    }

    pub fn stop_all_notes(&mut self) {
        for t in &mut self.tracks {
            t.all_notes_off();
        }
        self.master_track.all_notes_off();
    }

    // ── Track Management ─────────────────────────────────────────────────────

    pub fn add_track(&mut self, name: &str, kind: TrackKind, instrument_id: Option<&str>) -> String {
        self.push_undo();
        let id = self.alloc_id("track");
        let palette = [
            "#4C8DFF", "#E68A3E", "#9B6BFF", "#38B87C", "#2BB3C0", "#D95B82", "#D4A72C",
        ];
        let color = palette[self.tracks.len() % palette.len()].to_string();
        let mut rt = TrackRuntime::new(
            TrackSnapshot {
                id: id.clone(),
                name: name.to_string(),
                kind,
                color_hex: color.clone(),
                volume_db: -2.0,
                pan: 0.0,
                muted: false,
                soloed: false,
                armed: false,
                input_monitoring: false,
                automation_read: true,
                spatial_x: 0.0,
                spatial_y: 0.85,
                spatial_z: 0.0,
                spatial_binaural: false,
                instrument: None,
                inserts: Vec::new(),
                sends: Vec::new(),
                midi_clips: Vec::new(),
                audio_clips: Vec::new(),
                automation_lanes: Vec::new(),
            },
            self.sample_rate,
        );

        let default_inst = match kind {
            TrackKind::DrumKit => Some(instrument_id.unwrap_or("drumsampler")),
            TrackKind::Instrument => Some(instrument_id.unwrap_or("wrapsynth")),
            _ => None,
        };
        if let Some(inst_id) = default_inst {
            rt.instrument_instance = BuiltinPluginInstance::new(inst_id, self.sample_rate as f32);
            let clip_id = self.alloc_id("clip-midi");
            rt.snapshot.midi_clips.push(MidiClip {
                id: clip_id,
                name: format!("{name} Pattern"),
                start_beat: 0.0,
                length_beats: 8.0,
                loop_length_beats: 8.0,
                loop_enabled: true,
                color_hex: color,
                notes: vec![MidiNote {
                    id: self.alloc_id("note"),
                    pitch: if kind == TrackKind::DrumKit { 36 } else { 60 },
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    velocity: 100,
                    muted: false,
                }],
                muted: false,
            });
        }
        rt.sync_snapshot_from_plugins();
        self.tracks.push(rt);
        self.selected_track_id = id.clone();
        id
    }

    pub fn delete_track(&mut self, track_id: &str) -> bool {
        if track_id == "master" || self.tracks.len() <= 1 {
            return false;
        }
        let Some(pos) = self.tracks.iter().position(|t| t.snapshot.id == track_id) else {
            return false;
        };
        self.push_undo();
        self.tracks.remove(pos);
        if self.selected_track_id == track_id {
            self.selected_track_id = self.tracks[0].snapshot.id.clone();
        }
        true
    }

    fn find_track_mut(&mut self, track_id: &str) -> Option<&mut TrackRuntime> {
        if track_id == "master" {
            return Some(&mut self.master_track);
        }
        self.tracks.iter_mut().find(|t| t.snapshot.id == track_id)
    }

    pub fn set_track_mixer(
        &mut self,
        track_id: &str,
        volume_db: Option<f32>,
        pan: Option<f32>,
        muted: Option<bool>,
        soloed: Option<bool>,
        armed: Option<bool>,
        input_monitoring: Option<bool>,
    ) -> bool {
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        if let Some(v) = volume_db {
            t.snapshot.volume_db = v.clamp(-60.0, 12.0);
        }
        if let Some(p) = pan {
            t.snapshot.pan = p.clamp(-1.0, 1.0);
        }
        if let Some(m) = muted {
            t.snapshot.muted = m;
        }
        if let Some(s) = soloed {
            t.snapshot.soloed = s;
        }
        if let Some(a) = armed {
            t.snapshot.armed = a;
        }
        if let Some(im) = input_monitoring {
            t.snapshot.input_monitoring = im;
        }
        true
    }

    pub fn set_track_spatial(
        &mut self,
        track_id: &str,
        x: f32,
        y: f32,
        z: f32,
        binaural: bool,
    ) -> bool {
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        t.snapshot.spatial_x = x.clamp(-1.0, 1.0);
        t.snapshot.spatial_y = y.clamp(-1.0, 1.0);
        t.snapshot.spatial_z = z.clamp(0.0, 1.0);
        t.snapshot.spatial_binaural = binaural;
        true
    }

    pub fn set_track_send(
        &mut self,
        track_id: &str,
        bus_id: &str,
        level_db: f32,
        enabled: bool,
    ) -> bool {
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        if let Some(send) = t
            .snapshot
            .sends
            .iter_mut()
            .find(|s| s.target_bus_id == bus_id)
        {
            send.level_db = level_db.clamp(-60.0, 12.0);
            send.enabled = enabled;
        } else {
            t.snapshot.sends.push(SendSlot {
                target_bus_id: bus_id.to_string(),
                level_db: level_db.clamp(-60.0, 12.0),
                enabled,
                pre_fader: false,
            });
        }
        true
    }

    // ── Plugin Chain & Parameter Control ─────────────────────────────────────

    pub fn set_track_instrument(&mut self, track_id: &str, plugin_id: &str) -> bool {
        let sr = self.sample_rate as f32;
        let Some(inst) = BuiltinPluginInstance::new(plugin_id, sr) else {
            return false;
        };
        self.push_undo();
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        t.all_notes_off();
        t.instrument_instance = Some(inst);
        t.sync_snapshot_from_plugins();
        self.inspected_plugin_target = Some((track_id.to_string(), None));
        true
    }

    pub fn add_track_insert(&mut self, track_id: &str, plugin_id: &str) -> Option<usize> {
        let sr = self.sample_rate as f32;
        let inst = BuiltinPluginInstance::new(plugin_id, sr)?;
        self.push_undo();
        let t = self.find_track_mut(track_id)?;
        if t.insert_instances.len() >= 8 {
            return None;
        }
        t.insert_instances.push(inst);
        let idx = t.insert_instances.len() - 1;
        t.sync_snapshot_from_plugins();
        self.inspected_plugin_target = Some((track_id.to_string(), Some(idx)));
        Some(idx)
    }

    pub fn remove_track_insert(&mut self, track_id: &str, slot_index: usize) -> bool {
        self.push_undo();
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        if slot_index >= t.insert_instances.len() {
            return false;
        }
        t.insert_instances.remove(slot_index);
        t.sync_snapshot_from_plugins();
        true
    }

    pub fn set_plugin_enabled(
        &mut self,
        track_id: &str,
        slot_index: Option<usize>,
        enabled: bool,
    ) -> bool {
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        match slot_index {
            None => {
                if let Some(inst) = &mut t.instrument_instance {
                    inst.enabled = enabled;
                    t.sync_snapshot_from_plugins();
                    return true;
                }
            }
            Some(idx) => {
                if let Some(inst) = t.insert_instances.get_mut(idx) {
                    inst.enabled = enabled;
                    t.sync_snapshot_from_plugins();
                    return true;
                }
            }
        }
        false
    }

    pub fn set_plugin_param(
        &mut self,
        track_id: &str,
        slot_index: Option<usize>,
        param_id: &str,
        value: f32,
    ) -> bool {
        self.inspected_plugin_target = Some((track_id.to_string(), slot_index));
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        let ok = match slot_index {
            None => t
                .instrument_instance
                .as_mut()
                .map(|i| i.set_param(param_id, value))
                .unwrap_or(false),
            Some(idx) => t
                .insert_instances
                .get_mut(idx)
                .map(|i| i.set_param(param_id, value))
                .unwrap_or(false),
        };
        if ok {
            t.sync_snapshot_from_plugins();
        }
        ok
    }

    pub fn apply_plugin_preset(
        &mut self,
        track_id: &str,
        slot_index: Option<usize>,
        preset_name: &str,
    ) -> bool {
        self.push_undo();
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        let ok = match slot_index {
            None => t
                .instrument_instance
                .as_mut()
                .map(|i| i.apply_preset(preset_name))
                .unwrap_or(false),
            Some(idx) => t
                .insert_instances
                .get_mut(idx)
                .map(|i| i.apply_preset(preset_name))
                .unwrap_or(false),
        };
        if ok {
            t.sync_snapshot_from_plugins();
        }
        ok
    }

    // ── Clips, Piano Roll & Audio File Import ────────────────────────────────

    pub fn add_midi_clip(&mut self, track_id: &str, start_beat: f64, length_beats: f64) -> Option<String> {
        self.push_undo();
        let clip_id = self.alloc_id("clip-midi");
        let note_id = self.alloc_id("note");
        let t = self.find_track_mut(track_id)?;
        let pitch = if t.snapshot.kind == TrackKind::DrumKit {
            36
        } else {
            60
        };
        let color = t.snapshot.color_hex.clone();
        t.snapshot.midi_clips.push(MidiClip {
            id: clip_id.clone(),
            name: "MIDI Clip".to_string(),
            start_beat: start_beat.max(0.0),
            length_beats: length_beats.max(1.0),
            loop_length_beats: length_beats.max(1.0),
            loop_enabled: true,
            color_hex: color,
            notes: vec![MidiNote {
                id: note_id,
                pitch,
                start_beat: 0.0,
                duration_beats: 1.0,
                velocity: 100,
                muted: false,
            }],
            muted: false,
        });
        self.selected_clip_id = Some(clip_id.clone());
        Some(clip_id)
    }

    pub fn import_audio_file_to_track(
        &mut self,
        track_id: &str,
        file_path: &str,
        start_beat: f64,
    ) -> Result<String, String> {
        let path = Path::new(file_path);
        let decoded = audio_io::decode_audio_file_stereo(path, self.sample_rate)?;
        let duration_secs = decoded.frames as f64 / self.sample_rate as f64;
        let length_beats = (duration_secs * (self.bpm / 60.0)).max(1.0);
        let peaks = audio_io::compute_stereo_peaks(&decoded.left, &decoded.right, 128);

        self.push_undo();
        let src_id = self.alloc_id("audio-src");
        let clip_id = self.alloc_id("clip-audio");
        self.audio_pool.insert(src_id.clone(), Arc::new(decoded));

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Imported Audio.wav")
            .to_string();

        if let Some(t) = self.find_track_mut(track_id) {
            if let Some(inst) = &mut t.instrument_instance {
                let _ = inst.load_sample_file(path, None);
            }
            let color = t.snapshot.color_hex.clone();
            t.snapshot.audio_clips.push(AudioClip {
                id: clip_id.clone(),
                name: file_name,
                source_id: src_id,
                source_path: file_path.to_string(),
                start_beat: start_beat.max(0.0),
                length_beats,
                offset_beats: 0.0,
                gain_db: 0.0,
                pitch_semitones: 0.0,
                fade_in_beats: 0.05,
                fade_out_beats: 0.05,
                color_hex: color,
                waveform_peaks: peaks,
                muted: false,
            });
            self.selected_clip_id = Some(clip_id.clone());
            Ok(clip_id)
        } else {
            Err(format!("Track '{track_id}' not found"))
        }
    }

    pub fn update_clip_bounds(
        &mut self,
        track_id: &str,
        clip_id: &str,
        start_beat: f64,
        length_beats: f64,
        gain_db: Option<f32>,
        pitch_semitones: Option<f32>,
    ) -> bool {
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        if let Some(mc) = t.snapshot.midi_clips.iter_mut().find(|c| c.id == clip_id) {
            mc.start_beat = start_beat.max(0.0);
            mc.length_beats = length_beats.max(0.25);
            return true;
        }
        if let Some(ac) = t.snapshot.audio_clips.iter_mut().find(|c| c.id == clip_id) {
            ac.start_beat = start_beat.max(0.0);
            ac.length_beats = length_beats.max(0.25);
            if let Some(g) = gain_db {
                ac.gain_db = g.clamp(-36.0, 24.0);
            }
            if let Some(p) = pitch_semitones {
                ac.pitch_semitones = p.clamp(-24.0, 24.0);
            }
            return true;
        }
        false
    }

    pub fn split_clip(&mut self, track_id: &str, clip_id: &str, split_beat: f64) -> bool {
        self.push_undo();
        let new_id = self.alloc_id("clip-split");
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        if let Some(idx) = t.snapshot.midi_clips.iter().position(|c| c.id == clip_id) {
            let clip = &t.snapshot.midi_clips[idx];
            if split_beat <= clip.start_beat + 0.125
                || split_beat >= clip.start_beat + clip.length_beats - 0.125
            {
                return false;
            }
            let rel_split = split_beat - clip.start_beat;
            let right_len = clip.length_beats - rel_split;
            let mut right_clip = clip.clone();
            right_clip.id = new_id;
            right_clip.start_beat = split_beat;
            right_clip.length_beats = right_len;
            right_clip.notes = clip
                .notes
                .iter()
                .filter(|n| n.start_beat >= rel_split)
                .map(|n| {
                    let mut c = n.clone();
                    c.start_beat -= rel_split;
                    c
                })
                .collect();

            t.snapshot.midi_clips[idx].length_beats = rel_split;
            t.snapshot.midi_clips[idx]
                .notes
                .retain(|n| n.start_beat < rel_split);
            t.snapshot.midi_clips.push(right_clip);
            return true;
        }
        if let Some(idx) = t.snapshot.audio_clips.iter().position(|c| c.id == clip_id) {
            let clip = &t.snapshot.audio_clips[idx];
            if split_beat <= clip.start_beat + 0.125
                || split_beat >= clip.start_beat + clip.length_beats - 0.125
            {
                return false;
            }
            let rel_split = split_beat - clip.start_beat;
            let right_len = clip.length_beats - rel_split;
            let mut right_clip = clip.clone();
            right_clip.id = new_id;
            right_clip.start_beat = split_beat;
            right_clip.length_beats = right_len;
            right_clip.offset_beats += rel_split;

            t.snapshot.audio_clips[idx].length_beats = rel_split;
            t.snapshot.audio_clips.push(right_clip);
            return true;
        }
        false
    }

    pub fn delete_clip(&mut self, track_id: &str, clip_id: &str) -> bool {
        self.push_undo();
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        let before_m = t.snapshot.midi_clips.len();
        t.snapshot.midi_clips.retain(|c| c.id != clip_id);
        let before_a = t.snapshot.audio_clips.len();
        t.snapshot.audio_clips.retain(|c| c.id != clip_id);
        before_m != t.snapshot.midi_clips.len() || before_a != t.snapshot.audio_clips.len()
    }

    pub fn upsert_midi_note(
        &mut self,
        track_id: &str,
        clip_id: &str,
        note_id: Option<&str>,
        pitch: u8,
        start_beat: f64,
        duration_beats: f64,
        velocity: u8,
    ) -> Option<String> {
        let nid = note_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.alloc_id("note"));
        let t = self.find_track_mut(track_id)?;
        let clip = t.snapshot.midi_clips.iter_mut().find(|c| c.id == clip_id)?;
        if let Some(existing) = clip.notes.iter_mut().find(|n| n.id == nid) {
            existing.pitch = pitch.min(127);
            existing.start_beat = start_beat.max(0.0);
            existing.duration_beats = duration_beats.max(0.0625);
            existing.velocity = velocity.clamp(1, 127);
        } else {
            clip.notes.push(MidiNote {
                id: nid.clone(),
                pitch: pitch.min(127),
                start_beat: start_beat.max(0.0),
                duration_beats: duration_beats.max(0.0625),
                velocity: velocity.clamp(1, 127),
                muted: false,
            });
        }
        Some(nid)
    }

    pub fn delete_midi_note(&mut self, track_id: &str, clip_id: &str, note_id: &str) -> bool {
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        let Some(clip) = t.snapshot.midi_clips.iter_mut().find(|c| c.id == clip_id) else {
            return false;
        };
        let len_before = clip.notes.len();
        clip.notes.retain(|n| n.id != note_id);
        len_before != clip.notes.len()
    }

    pub fn quantize_midi_clip(&mut self, track_id: &str, clip_id: &str, grid_beats: f64) -> bool {
        let grid = grid_beats.clamp(0.0625, 4.0);
        self.push_undo();
        let Some(t) = self.find_track_mut(track_id) else {
            return false;
        };
        let Some(clip) = t.snapshot.midi_clips.iter_mut().find(|c| c.id == clip_id) else {
            return false;
        };
        for note in &mut clip.notes {
            note.start_beat = (note.start_beat / grid).round() * grid;
            note.duration_beats = ((note.duration_beats / grid).round().max(1.0)) * grid;
        }
        true
    }

    pub fn trigger_live_midi(
        &mut self,
        track_id: &str,
        note: u8,
        velocity: u8,
        is_note_on: bool,
    ) {
        let target = if track_id.is_empty() {
            self.selected_track_id.clone()
        } else {
            track_id.to_string()
        };
        self.live_midi_queue.push(LiveMidiEvent {
            track_id: target,
            note: note.min(127),
            velocity: velocity.min(127),
            is_note_on,
        });
    }

    // ── Real-Time Stereo Audio Graph Render ──────────────────────────────────

    pub fn render_interleaved_stereo(&mut self, out_interleaved: &mut [f32], frames: usize) {
        let n = frames.min(out_interleaved.len() / 2);
        if n == 0 {
            return;
        }
        let t0 = std::time::Instant::now();

        // 1. Dispatch queued live MIDI events
        let events: Vec<LiveMidiEvent> = self.live_midi_queue.drain(..).collect();
        for ev in events {
            if let Some(tr) = self.tracks.iter_mut().find(|t| t.snapshot.id == ev.track_id) {
                if let Some(inst) = &mut tr.instrument_instance {
                    if ev.is_note_on && ev.velocity > 0 {
                        inst.note_on(ev.note, ev.velocity as f32 / 127.0);
                    } else {
                        inst.note_off(ev.note);
                    }
                }
            }
        }

        let sr = self.sample_rate as f64;
        let beats_per_frame = (self.bpm / 60.0) / sr;
        let block_start_beat = self.playhead_beats;
        let mut block_end_beat = block_start_beat + beats_per_frame * (n as f64);

        if self.is_playing && self.loop_enabled && block_end_beat >= self.loop_end_beats {
            let overshoot = block_end_beat - self.loop_end_beats;
            block_end_beat = self.loop_start_beats + overshoot;
            for tr in &mut self.tracks {
                tr.all_notes_off();
            }
        }

        let any_solo = self.tracks.iter().any(|t| t.snapshot.soloed);

        // Prepare bus accumulation buffers
        for tr in &mut self.tracks {
            if tr.buf_l.len() < n {
                tr.buf_l.resize(n, 0.0);
                tr.buf_r.resize(n, 0.0);
            }
            tr.buf_l[..n].fill(0.0);
            tr.buf_r[..n].fill(0.0);
        }

        // 2. Render non-bus tracks (Audio, Instrument, DrumKit)
        let mut bus_accum: HashMap<String, (Vec<f32>, Vec<f32>)> = HashMap::new();
        for tr in &self.tracks {
            if tr.snapshot.kind == TrackKind::Bus {
                bus_accum.insert(tr.snapshot.id.clone(), (vec![0.0; n], vec![0.0; n]));
            }
        }

        for tr in &mut self.tracks {
            if tr.snapshot.kind == TrackKind::Bus {
                continue;
            }
            let audible = !tr.snapshot.muted && (!any_solo || tr.snapshot.soloed);

            // Evaluate automation lanes at block_start_beat
            if tr.snapshot.automation_read {
                for lane in &tr.snapshot.automation_lanes {
                    if !lane.enabled || lane.points.is_empty() {
                        continue;
                    }
                    let val = eval_automation_points(&lane.points, block_start_beat);
                    if lane.target_param == "volume_db" {
                        tr.snapshot.volume_db = val.clamp(-60.0, 12.0);
                    } else if lane.target_param == "pan" {
                        tr.snapshot.pan = val.clamp(-1.0, 1.0);
                    }
                }
            }

            // 2a. Schedule MIDI clip notes if playing
            if self.is_playing {
                let mut desired_notes = [0u8; 128];
                for clip in &tr.snapshot.midi_clips {
                    if clip.muted {
                        continue;
                    }
                    if block_start_beat >= clip.start_beat
                        && block_start_beat < clip.start_beat + clip.length_beats
                    {
                        let mut rel_beat = block_start_beat - clip.start_beat;
                        if clip.loop_enabled && clip.loop_length_beats > 0.0625 {
                            rel_beat %= clip.loop_length_beats;
                        }
                        for note in &clip.notes {
                            if note.muted {
                                continue;
                            }
                            if rel_beat >= note.start_beat
                                && rel_beat < note.start_beat + note.duration_beats
                            {
                                let p = (note.pitch as usize).min(127);
                                desired_notes[p] = desired_notes[p].max(note.velocity.max(1));
                            }
                        }
                    }
                }
                if let Some(inst) = &mut tr.instrument_instance {
                    for pitch in 0..128 {
                        let want_vel = desired_notes[pitch];
                        let is_on = tr.active_midi_notes[pitch];
                        if want_vel > 0 && !is_on {
                            inst.note_on(pitch as u8, want_vel as f32 / 127.0);
                            tr.active_midi_notes[pitch] = true;
                        } else if want_vel == 0 && is_on {
                            inst.note_off(pitch as u8);
                            tr.active_midi_notes[pitch] = false;
                        }
                    }
                }
            }

            // 2b. Synthesize instrument
            if let Some(inst) = &mut tr.instrument_instance {
                inst.process_stereo_block(&mut tr.buf_l[..n], &mut tr.buf_r[..n]);
            }

            // 2c. Render Audio Clips
            if self.is_playing && !tr.snapshot.audio_clips.is_empty() {
                for clip in &tr.snapshot.audio_clips {
                    if clip.muted {
                        continue;
                    }
                    let Some(source) = self.audio_pool.get(&clip.source_id) else {
                        continue;
                    };
                    let clip_gain = 10.0f32.powf(clip.gain_db / 20.0);
                    let pitch_ratio = 2.0f64.powf(clip.pitch_semitones as f64 / 12.0);
                    for i in 0..n {
                        let beat = block_start_beat + beats_per_frame * (i as f64);
                        if beat < clip.start_beat || beat >= clip.start_beat + clip.length_beats {
                            continue;
                        }
                        let rel_beat = beat - clip.start_beat + clip.offset_beats;
                        let sec = rel_beat * (60.0 / self.bpm) * pitch_ratio;
                        let src_frame = sec * (source.sample_rate as f64);
                        let idx0 = src_frame.floor() as usize;
                        if idx0 + 1 >= source.frames {
                            continue;
                        }
                        let frac = (src_frame - idx0 as f64) as f32;
                        let sl = source.left[idx0] * (1.0 - frac) + source.left[idx0 + 1] * frac;
                        let sr_val =
                            source.right[idx0] * (1.0 - frac) + source.right[idx0 + 1] * frac;

                        // Fade envelope
                        let pos_in_clip = beat - clip.start_beat;
                        let rem_in_clip = (clip.start_beat + clip.length_beats) - beat;
                        let mut env = 1.0f32;
                        if clip.fade_in_beats > 0.001 && pos_in_clip < clip.fade_in_beats {
                            env *= (pos_in_clip / clip.fade_in_beats) as f32;
                        }
                        if clip.fade_out_beats > 0.001 && rem_in_clip < clip.fade_out_beats {
                            env *= (rem_in_clip / clip.fade_out_beats) as f32;
                        }

                        tr.buf_l[i] += sl * clip_gain * env;
                        tr.buf_r[i] += sr_val * clip_gain * env;
                    }
                }
            }

            // 2d. Process track insert chain
            let mut max_gr = 0.0f32;
            for insert in &mut tr.insert_instances {
                insert.process_stereo_block(&mut tr.buf_l[..n], &mut tr.buf_r[..n]);
                max_gr = max_gr.max(insert.last_gr_db);
            }
            tr.gr_db = max_gr;

            // 2e. Apply track volume, pan / binaural spatializer
            if !audible {
                tr.buf_l[..n].fill(0.0);
                tr.buf_r[..n].fill(0.0);
                tr.peak_l *= 0.7;
                tr.peak_r *= 0.7;
                tr.rms_l *= 0.7;
                tr.rms_r *= 0.7;
                continue;
            }

            let gain = 10.0f32.powf(tr.snapshot.volume_db / 20.0);
            if tr.snapshot.spatial_binaural || self.master_binaural_enabled {
                let mut sp_l = vec![0.0f32; n];
                let mut sp_r = vec![0.0f32; n];
                let params = SourceParams {
                    position: RoomPosition::new(
                        tr.snapshot.spatial_x,
                        tr.snapshot.spatial_y,
                        tr.snapshot.spatial_z,
                    ),
                    spread: 0.15,
                    width: 0.85,
                    lfe: 0.0,
                };
                tr.binaural_source.process(
                    &tr.buf_l[..n],
                    &tr.buf_r[..n],
                    &params,
                    &self.room_settings,
                    &mut sp_l,
                    &mut sp_r,
                );
                for i in 0..n {
                    tr.buf_l[i] = sp_l[i] * gain;
                    tr.buf_r[i] = sp_r[i] * gain;
                }
            } else {
                let angle = (tr.snapshot.pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
                let gl = angle.cos() * gain;
                let gr = angle.sin() * gain;
                for i in 0..n {
                    tr.buf_l[i] *= gl;
                    tr.buf_r[i] *= gr;
                }
            }

            // Compute track meters
            update_track_meters(tr, n);

            // 2f. Route sends to buses
            for send in &tr.snapshot.sends {
                if !send.enabled {
                    continue;
                }
                if let Some((bl, br)) = bus_accum.get_mut(&send.target_bus_id) {
                    let sg = 10.0f32.powf(send.level_db / 20.0);
                    for i in 0..n {
                        bl[i] += tr.buf_l[i] * sg;
                        br[i] += tr.buf_r[i] * sg;
                    }
                }
            }
        }

        // 3. Render Bus tracks
        for tr in &mut self.tracks {
            if tr.snapshot.kind != TrackKind::Bus {
                continue;
            }
            let audible = !tr.snapshot.muted && (!any_solo || tr.snapshot.soloed);
            if let Some((bl, br)) = bus_accum.get(&tr.snapshot.id) {
                tr.buf_l[..n].copy_from_slice(&bl[..n]);
                tr.buf_r[..n].copy_from_slice(&br[..n]);
            }
            let mut max_gr = 0.0f32;
            for insert in &mut tr.insert_instances {
                insert.process_stereo_block(&mut tr.buf_l[..n], &mut tr.buf_r[..n]);
                max_gr = max_gr.max(insert.last_gr_db);
            }
            tr.gr_db = max_gr;
            if !audible {
                tr.buf_l[..n].fill(0.0);
                tr.buf_r[..n].fill(0.0);
            } else {
                let gain = 10.0f32.powf(tr.snapshot.volume_db / 20.0);
                let angle = (tr.snapshot.pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
                let gl = angle.cos() * gain;
                let gr = angle.sin() * gain;
                for i in 0..n {
                    tr.buf_l[i] *= gl;
                    tr.buf_r[i] *= gr;
                }
            }
            update_track_meters(tr, n);
        }

        // 4. Sum all audible tracks into Master Bus
        if self.master_track.buf_l.len() < n {
            self.master_track.buf_l.resize(n, 0.0);
            self.master_track.buf_r.resize(n, 0.0);
        }
        self.master_track.buf_l[..n].fill(0.0);
        self.master_track.buf_r[..n].fill(0.0);

        for tr in &self.tracks {
            for i in 0..n {
                self.master_track.buf_l[i] += tr.buf_l[i];
                self.master_track.buf_r[i] += tr.buf_r[i];
            }
        }

        // 5. Process Master Insert Chain
        let mut master_gr = 0.0f32;
        for insert in &mut self.master_track.insert_instances {
            insert.process_stereo_block(
                &mut self.master_track.buf_l[..n],
                &mut self.master_track.buf_r[..n],
            );
            master_gr = master_gr.max(insert.last_gr_db);
        }
        self.master_track.gr_db = master_gr;

        let master_gain = if self.master_track.snapshot.muted {
            0.0
        } else {
            10.0f32.powf(self.master_track.snapshot.volume_db / 20.0)
        };
        for i in 0..n {
            self.master_track.buf_l[i] =
                (self.master_track.buf_l[i] * master_gain).clamp(-1.0, 1.0);
            self.master_track.buf_r[i] =
                (self.master_track.buf_r[i] * master_gain).clamp(-1.0, 1.0);
        }

        // 6. Optional Metronome Click
        if self.is_playing && self.metronome_enabled {
            for i in 0..n {
                let prev_b = block_start_beat + beats_per_frame * (i as f64);
                let next_b = prev_b + beats_per_frame;
                if prev_b.floor() != next_b.floor() {
                    let beat_num = (next_b.floor() as u32) % self.time_sig_num.max(1);
                    self.metronome_env = 1.0;
                    self.metronome_freq = if beat_num == 0 { 1560.0 } else { 980.0 };
                }
                if self.metronome_env > 0.001 {
                    let click = (self.metronome_phase * std::f32::consts::TAU).sin()
                        * self.metronome_env
                        * 0.22;
                    self.metronome_phase =
                        (self.metronome_phase + self.metronome_freq / (sr as f32)).fract();
                    self.metronome_env *= 0.9965;
                    self.master_track.buf_l[i] =
                        (self.master_track.buf_l[i] + click).clamp(-1.0, 1.0);
                    self.master_track.buf_r[i] =
                        (self.master_track.buf_r[i] + click).clamp(-1.0, 1.0);
                }
            }
        }

        update_track_meters(&mut self.master_track, n);

        // 7. Feed Spectrum Analyzer + EBU R128 + write output
        if self.interleaved_meter_scratch.len() < n * 2 {
            self.interleaved_meter_scratch.resize(n * 2, 0.0);
        }
        for i in 0..n {
            let l = self.master_track.buf_l[i];
            let r = self.master_track.buf_r[i];
            out_interleaved[i * 2] = l;
            out_interleaved[i * 2 + 1] = r;
            self.interleaved_meter_scratch[i * 2] = l;
            self.interleaved_meter_scratch[i * 2 + 1] = r;
        }
        self.spectrum.push_block(
            &self.master_track.buf_l[..n],
            &self.master_track.buf_r[..n],
        );
        let _ = self.spectrum.analyze();
        self.spectrum_bins_cache = *self.spectrum.levels();
        if let Some(ebu) = &mut self.ebur128 {
            let _ = ebu.add_frames_f32(&self.interleaved_meter_scratch[..n * 2]);
        }

        if self.is_playing {
            self.playhead_beats = block_end_beat;
        }

        let elapsed_s = t0.elapsed().as_secs_f32();
        let budget_s = (n as f32) / (self.sample_rate as f32);
        let load = ((elapsed_s / budget_s.max(1.0e-5)) * 100.0).clamp(1.2, 99.0);
        self.last_cpu_load_pct = self.last_cpu_load_pct * 0.85 + load * 0.15;
    }

    pub fn telemetry(&self) -> TelemetrySnapshot {
        let mut lufs_m = -70.0f32;
        let mut lufs_s = -70.0f32;
        let mut lufs_i = -70.0f32;
        let mut dbtp = -96.0f32;

        if let Some(ebu) = &self.ebur128 {
            if let Ok(v) = ebu.loudness_momentary() {
                if v.is_finite() {
                    lufs_m = (v as f32).clamp(-70.0, 6.0);
                }
            }
            if let Ok(v) = ebu.loudness_shortterm() {
                if v.is_finite() {
                    lufs_s = (v as f32).clamp(-70.0, 6.0);
                }
            }
            if let Ok(v) = ebu.loudness_global() {
                if v.is_finite() {
                    lufs_i = (v as f32).clamp(-70.0, 6.0);
                }
            }
            let tp_l = ebu.true_peak(0).unwrap_or(0.0);
            let tp_r = ebu.true_peak(1).unwrap_or(0.0);
            let tp = tp_l.max(tp_r) as f32;
            if tp > 1.0e-5 {
                dbtp = (20.0 * tp.log10()).clamp(-96.0, 12.0);
            }
        }

        let mut track_meters = Vec::with_capacity(self.tracks.len() + 1);
        for tr in &self.tracks {
            track_meters.push(TrackMeterFrame {
                track_id: tr.snapshot.id.clone(),
                peak_l_db: linear_to_db(tr.peak_l),
                peak_r_db: linear_to_db(tr.peak_r),
                rms_l_db: linear_to_db(tr.rms_l),
                rms_r_db: linear_to_db(tr.rms_r),
                gain_reduction_db: tr.gr_db,
                clipping: tr.peak_l > 0.99 || tr.peak_r > 0.99,
            });
        }

        let inspected_plugin_meter =
            self.inspected_plugin_target
                .as_ref()
                .and_then(|(tid, slot)| {
                    let tr = if tid == "master" {
                        Some(&self.master_track)
                    } else {
                        self.tracks.iter().find(|t| &t.snapshot.id == tid)
                    }?;
                    match slot {
                        None => tr.instrument_instance.as_ref().map(|i| i.meter_snapshot()),
                        Some(idx) => tr
                            .insert_instances
                            .get(*idx)
                            .map(|i| i.meter_snapshot()),
                    }
                });

        TelemetrySnapshot {
            playhead_beats: self.playhead_beats,
            is_playing: self.is_playing,
            is_recording: self.is_recording,
            bpm: self.bpm,
            master_peak_l_db: linear_to_db(self.master_track.peak_l),
            master_peak_r_db: linear_to_db(self.master_track.peak_r),
            master_rms_l_db: linear_to_db(self.master_track.rms_l),
            master_rms_r_db: linear_to_db(self.master_track.rms_r),
            lufs_momentary: lufs_m,
            lufs_short_term: lufs_s,
            lufs_integrated: lufs_i,
            true_peak_dbtp: dbtp,
            cpu_load_pct: self.last_cpu_load_pct,
            spectrum_db: self.spectrum_bins_cache.to_vec(),
            track_meters,
            inspected_plugin_meter,
        }
    }

    pub fn export_wav(&mut self, output_path: &str, total_beats: f64) -> Result<(), String> {
        let was_playing = self.is_playing;
        let saved_playhead = self.playhead_beats;
        let saved_loop = self.loop_enabled;

        self.stop_all_notes();
        self.playhead_beats = 0.0;
        self.loop_enabled = false;
        self.is_playing = true;

        let duration_secs = (total_beats.max(1.0)) * (60.0 / self.bpm);
        let total_frames = ((duration_secs * self.sample_rate as f64) as usize).max(1024);
        let mut left = Vec::with_capacity(total_frames);
        let mut right = Vec::with_capacity(total_frames);
        let mut block = vec![0.0f32; 1024];

        let mut remaining = total_frames;
        while remaining > 0 {
            let chunk = remaining.min(512);
            self.render_interleaved_stereo(&mut block[..chunk * 2], chunk);
            for i in 0..chunk {
                left.push(block[i * 2]);
                right.push(block[i * 2 + 1]);
            }
            remaining -= chunk;
        }

        self.is_playing = was_playing;
        self.playhead_beats = saved_playhead;
        self.loop_enabled = saved_loop;
        self.stop_all_notes();

        let wav_bytes = audio_io::encode_stereo_wav_i16(&left, &right, self.sample_rate);
        std::fs::write(Path::new(output_path), wav_bytes)
            .map_err(|e| format!("Failed to write WAV '{}': {e}", output_path))
    }
}

fn update_track_meters(tr: &mut TrackRuntime, frames: usize) {
    let mut pl = 0.0f32;
    let mut pr = 0.0f32;
    let mut sum_l = 0.0f32;
    let mut sum_r = 0.0f32;
    for i in 0..frames {
        let l = tr.buf_l[i].abs();
        let r = tr.buf_r[i].abs();
        pl = pl.max(l);
        pr = pr.max(r);
        sum_l += l * l;
        sum_r += r * r;
    }
    let rl = (sum_l / frames.max(1) as f32).sqrt();
    let rr = (sum_r / frames.max(1) as f32).sqrt();
    tr.peak_l = if pl > tr.peak_l {
        pl
    } else {
        tr.peak_l * 0.86 + pl * 0.14
    };
    tr.peak_r = if pr > tr.peak_r {
        pr
    } else {
        tr.peak_r * 0.86 + pr * 0.14
    };
    tr.rms_l = tr.rms_l * 0.85 + rl * 0.15;
    tr.rms_r = tr.rms_r * 0.85 + rr * 0.15;
}

#[inline]
fn linear_to_db(lin: f32) -> f32 {
    if lin <= 1.0e-5 {
        -96.0
    } else {
        (20.0 * lin.log10()).clamp(-96.0, 12.0)
    }
}

fn eval_automation_points(points: &[AutomationPoint], beat: f64) -> f32 {
    if points.is_empty() {
        return 0.0;
    }
    if beat <= points[0].beat {
        return points[0].value;
    }
    for w in points.windows(2) {
        let a = &w[0];
        let b = &w[1];
        if beat >= a.beat && beat <= b.beat {
            let span = (b.beat - a.beat).max(1.0e-6);
            let t = ((beat - a.beat) / span) as f32;
            return a.value + (b.value - a.value) * t;
        }
    }
    points.last().map(|p| p.value).unwrap_or(0.0)
}
