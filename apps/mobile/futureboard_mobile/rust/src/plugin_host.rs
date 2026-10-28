//! Unified built-in plugin host for Futureboard Studio Mobile.
//!
//! Wraps all 22 BuiltinAudioPlugins crates + `SphereAudioPlugins` + `SphereSoundfontPlayer`
//! into a uniform, allocation-free-on-audio-thread processor with full parameter reflection,
//! factory presets, EQ frequency response calculation, and per-plugin telemetry.

use std::collections::HashMap;
use std::path::Path;

use builtin_dsp_core::{Instrument, StereoEffect};
use serde::{Deserialize, Serialize};
use sphere_audio_plugins::{AudioPluginDspState, PluginParams};
use sphere_soundfont_player::{SoundfontPlayer, SoundfontPlayerSettings};

use crate::audio_io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PluginKind {
    Instrument,
    Effect,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginParamSpec {
    pub id: String,
    pub name: String,
    pub value: f32,
    pub default_value: f32,
    pub min: f32,
    pub max: f32,
    pub unit: String,
    pub group: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginCatalogEntry {
    pub id: String,
    pub name: String,
    pub category: String,
    pub kind: PluginKind,
    pub description: String,
    pub presets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMeterSnapshot {
    pub input_peak_db: f32,
    pub output_peak_db: f32,
    pub gain_reduction_db: f32,
    pub pad_levels: Vec<f32>,
    pub eq_curve_db: Vec<f32>,
}

impl Default for PluginMeterSnapshot {
    fn default() -> Self {
        Self {
            input_peak_db: -96.0,
            output_peak_db: -96.0,
            gain_reduction_db: 0.0,
            pad_levels: Vec::new(),
            eq_curve_db: Vec::new(),
        }
    }
}

pub enum BuiltinDsp {
    // ── Effects ──────────────────────────────────────────────────────────
    Equz8(Box<equz8::Dsp>),
    Equzx(Box<equzx::Dsp>),
    Compresser(Box<compresser::Dsp>),
    Fa2a(Box<fa2a::Dsp>),
    Fa76(Box<fa76::Dsp>),
    Zcomp(Box<zcomp::Dsp>),
    Echospace(Box<echospace::Dsp>),
    Verbspace(Box<verbspace::Dsp>),
    Imager(Box<imager::Dsp>),
    Burnlimit(Box<burnlimit::Dsp>),
    Clipper67(Box<clipper67::Dsp>),
    Transient(Box<transient::Dsp>),
    Waygate(Box<waygate::Dsp>),
    Drumsilencer(Box<drumsilencer::Dsp>),
    C1073(Box<c1073::Dsp>),
    Mixstation(Box<mixstation::Dsp>),
    Whitesharp(Box<whitesharp::Dsp>),
    Rodharerist(Box<rodharerist::Dsp>),
    SpherePlugin {
        plugin_id: String,
        params: PluginParams,
        state: AudioPluginDspState,
    },
    // ── Instruments ──────────────────────────────────────────────────────
    Wrapsynth {
        dsp: Box<wrapsynth::Dsp>,
        scratch: Vec<f32>,
    },
    Meowsyn(Box<meowsyn::Dsp>),
    Drumsampler(Box<drumsampler::Dsp>),
    Quicksampler {
        dsp: Box<quicksampler::Dsp>,
        scratch: Vec<f32>,
    },
    Slicer {
        dsp: Box<slicer::Dsp>,
        scratch: Vec<f32>,
    },
    Soundfont {
        player: Box<SoundfontPlayer>,
        left_buf: Vec<f32>,
        right_buf: Vec<f32>,
        gain_db: f32,
        pan: f32,
        transpose: i32,
    },
}

pub struct BuiltinPluginInstance {
    pub plugin_id: String,
    pub name: String,
    pub kind: PluginKind,
    pub enabled: bool,
    pub sample_rate: f32,
    pub dsp: BuiltinDsp,
    pub param_values: HashMap<String, f32>,
    pub param_specs: Vec<PluginParamSpec>,
    pub active_preset: String,
    pub last_in_peak: f32,
    pub last_out_peak: f32,
    pub last_gr_db: f32,
}

impl BuiltinPluginInstance {
    pub fn new(plugin_id: &str, sample_rate: f32) -> Option<Self> {
        let canonical = canonicalize_id(plugin_id);
        let sr = sample_rate.clamp(8_000.0, 192_000.0);
        let (name, kind, dsp) = match canonical.as_str() {
            "equz8" => (
                "EQ-Z8 Parametric EQ",
                PluginKind::Effect,
                BuiltinDsp::Equz8(Box::new(equz8::Dsp::new(sr))),
            ),
            "equzx" => (
                "EQ-ZX Mastering EQ",
                PluginKind::Effect,
                BuiltinDsp::Equzx(Box::new(equzx::Dsp::new(sr))),
            ),
            "compresser" => (
                "Compresser Studio Bus",
                PluginKind::Effect,
                BuiltinDsp::Compresser(Box::new(compresser::Dsp::new(sr))),
            ),
            "fa2a" => (
                "FA-2A Optical Leveler",
                PluginKind::Effect,
                BuiltinDsp::Fa2a(Box::new(fa2a::Dsp::new(sr))),
            ),
            "fa76" => (
                "FA-76 FET Limiter",
                PluginKind::Effect,
                BuiltinDsp::Fa76(Box::new(fa76::Dsp::new(sr))),
            ),
            "zcomp" => (
                "Z-Comp VCA Dynamics",
                PluginKind::Effect,
                BuiltinDsp::Zcomp(Box::new(zcomp::Dsp::new(sr))),
            ),
            "echospace" => (
                "EchoSpace Tape Delay",
                PluginKind::Effect,
                BuiltinDsp::Echospace(Box::new(echospace::Dsp::new(sr))),
            ),
            "verbspace" => (
                "VerbSpace Algorithmic Reverb",
                PluginKind::Effect,
                BuiltinDsp::Verbspace(Box::new(verbspace::Dsp::new(sr))),
            ),
            "imager" => (
                "Imager Stereo Field",
                PluginKind::Effect,
                BuiltinDsp::Imager(Box::new(imager::Dsp::new(sr))),
            ),
            "burnlimit" => (
                "BurnLimit Mastering Limiter",
                PluginKind::Effect,
                BuiltinDsp::Burnlimit(Box::new(burnlimit::Dsp::new(sr))),
            ),
            "67clipper" => (
                "67Clipper Soft/Hard Clipper",
                PluginKind::Effect,
                BuiltinDsp::Clipper67(Box::new(clipper67::Dsp::new(sr))),
            ),
            "transient" => (
                "Transient Shaper",
                PluginKind::Effect,
                BuiltinDsp::Transient(Box::new(transient::Dsp::new(sr))),
            ),
            "waygate" => (
                "WayGate Noise Gate",
                PluginKind::Effect,
                BuiltinDsp::Waygate(Box::new(waygate::Dsp::new(sr))),
            ),
            "drumsilencer" => (
                "DrumSilencer De-Bleed",
                PluginKind::Effect,
                BuiltinDsp::Drumsilencer(Box::new(drumsilencer::Dsp::new(sr))),
            ),
            "c1073" => (
                "C-1073 Console Preamp/EQ",
                PluginKind::Effect,
                BuiltinDsp::C1073(Box::new(c1073::Dsp::new(sr))),
            ),
            "mixstation" => (
                "MixStation Channel Strip",
                PluginKind::Effect,
                BuiltinDsp::Mixstation(Box::new(mixstation::Dsp::new(sr))),
            ),
            "whitesharp" => (
                "WhiteSharp Tube Overdrive",
                PluginKind::Effect,
                BuiltinDsp::Whitesharp(Box::new(whitesharp::Dsp::new(sr))),
            ),
            "rodharerist" => (
                "Rodhareist Amp & Cab Suite",
                PluginKind::Effect,
                BuiltinDsp::Rodharerist(Box::new(rodharerist::Dsp::new(sr))),
            ),
            "sphere.eq8" | "sphere.drive" | "sphere.comp" => {
                let mut params = PluginParams::new();
                params.insert("power".into(), serde_json::json!(true));
                params.insert("mix".into(), serde_json::json!(100.0));
                params.insert("outputDb".into(), serde_json::json!(0.0));
                if canonical == "sphere.drive" {
                    params.insert("drive".into(), serde_json::json!(35.0));
                } else if canonical == "sphere.comp" {
                    params.insert("peakReduction".into(), serde_json::json!(40.0));
                } else {
                    for b in 1..=8 {
                        params.insert(format!("band{b}Active"), serde_json::json!(true));
                        params.insert(
                            format!("band{b}Type"),
                            serde_json::json!(if b == 1 {
                                "lowshelf"
                            } else if b == 8 {
                                "highshelf"
                            } else {
                                "bell"
                            }),
                        );
                        let default_freq = match b {
                            1 => 60.0,
                            2 => 150.0,
                            3 => 350.0,
                            4 => 800.0,
                            5 => 1800.0,
                            6 => 4000.0,
                            7 => 8500.0,
                            _ => 14000.0,
                        };
                        params.insert(format!("band{b}Freq"), serde_json::json!(default_freq));
                        params.insert(format!("band{b}Gain"), serde_json::json!(0.0));
                        params.insert(format!("band{b}Q"), serde_json::json!(1.0));
                    }
                }
                let state = AudioPluginDspState::new(&canonical, &params, sr as u32);
                let title = match canonical.as_str() {
                    "sphere.eq8" => "Sphere EQ8 Surgical",
                    "sphere.drive" => "Sphere Harmonic Drive",
                    _ => "Sphere Bus Compressor",
                };
                (
                    title,
                    PluginKind::Effect,
                    BuiltinDsp::SpherePlugin {
                        plugin_id: canonical.clone(),
                        params,
                        state,
                    },
                )
            }
            "wrapsynth" => (
                "WrapSynth Wavetable/VA Synth",
                PluginKind::Instrument,
                BuiltinDsp::Wrapsynth {
                    dsp: Box::new(wrapsynth::Dsp::new(sr)),
                    scratch: vec![0.0; 4096],
                },
            ),
            "meowsyn" => (
                "MeowSyn Poly Synth",
                PluginKind::Instrument,
                BuiltinDsp::Meowsyn(Box::new(meowsyn::Dsp::new(sr))),
            ),
            "drumsampler" => {
                let mut dsp = Box::new(drumsampler::Dsp::new(sr));
                let loaders: Vec<drumsampler::PadLoader> =
                    (0..16).filter_map(|i| dsp.pad_loader(i)).collect();
                audio_io::populate_default_drum_kit(&mut dsp, &loaders, sr);
                (
                    "Drum Sampler 64-Pad MPC",
                    PluginKind::Instrument,
                    BuiltinDsp::Drumsampler(dsp),
                )
            }
            "quicksampler" => {
                let dsp = Box::new(quicksampler::Dsp::new(sr));
                let sample = audio_io::synthesize_default_quicksampler_data(sr as u32);
                dsp.sample_loader().submit(Some(sample));
                (
                    "QuickSampler Chromatic Keyzone",
                    PluginKind::Instrument,
                    BuiltinDsp::Quicksampler {
                        dsp,
                        scratch: vec![0.0; 4096],
                    },
                )
            }
            "slicer" => {
                let dsp = Box::new(slicer::Dsp::new(sr));
                let sample = audio_io::synthesize_default_slicer_data(sr as u32);
                dsp.sample_loader().submit(Some(sample));
                (
                    "Slicer Beat Chopper",
                    PluginKind::Instrument,
                    BuiltinDsp::Slicer {
                        dsp,
                        scratch: vec![0.0; 4096],
                    },
                )
            }
            "soundfont" => {
                let sf = sphere_soundfont_player::test_font::sound_font();
                let settings = SoundfontPlayerSettings {
                    sample_rate: sr as i32,
                    ..SoundfontPlayerSettings::default()
                };
                let player = SoundfontPlayer::from_sound_font(sf, settings).ok()?;
                (
                    "Sphere SoundFont GM Player",
                    PluginKind::Instrument,
                    BuiltinDsp::Soundfont {
                        player: Box::new(player),
                        left_buf: vec![0.0; 2048],
                        right_buf: vec![0.0; 2048],
                        gain_db: 0.0,
                        pan: 0.0,
                        transpose: 0,
                    },
                )
            }
            _ => return None,
        };

        let specs = build_param_specs_for(&canonical);
        let mut param_values = HashMap::new();
        for spec in &specs {
            param_values.insert(spec.id.clone(), spec.default_value);
        }

        let mut inst = Self {
            plugin_id: canonical,
            name: name.to_string(),
            kind,
            enabled: true,
            sample_rate: sr,
            dsp,
            param_values,
            param_specs: specs,
            active_preset: "Default".to_string(),
            last_in_peak: 0.0,
            last_out_peak: 0.0,
            last_gr_db: 0.0,
        };
        // Sync defaults into DSP
        let defaults: Vec<(String, f32)> = inst
            .param_specs
            .iter()
            .map(|s| (s.id.clone(), s.default_value))
            .collect();
        for (id, val) in defaults {
            inst.set_param(&id, val);
        }
        Some(inst)
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        let sr = sample_rate.clamp(8_000.0, 192_000.0);
        self.sample_rate = sr;
        match &mut self.dsp {
            BuiltinDsp::Equz8(d) => d.set_sample_rate(sr),
            BuiltinDsp::Equzx(d) => d.set_sample_rate(sr),
            BuiltinDsp::Compresser(d) => d.set_sample_rate(sr),
            BuiltinDsp::Fa2a(d) => d.set_sample_rate(sr),
            BuiltinDsp::Fa76(d) => d.set_sample_rate(sr),
            BuiltinDsp::Zcomp(d) => d.set_sample_rate(sr),
            BuiltinDsp::Echospace(d) => d.set_sample_rate(sr),
            BuiltinDsp::Verbspace(d) => d.set_sample_rate(sr),
            BuiltinDsp::Imager(d) => d.set_sample_rate(sr),
            BuiltinDsp::Burnlimit(d) => d.set_sample_rate(sr),
            BuiltinDsp::Clipper67(d) => d.set_sample_rate(sr),
            BuiltinDsp::Transient(d) => d.set_sample_rate(sr),
            BuiltinDsp::Waygate(d) => d.set_sample_rate(sr),
            BuiltinDsp::Drumsilencer(d) => d.set_sample_rate(sr),
            BuiltinDsp::C1073(d) => d.set_sample_rate(sr),
            BuiltinDsp::Mixstation(d) => d.set_sample_rate(sr),
            BuiltinDsp::Whitesharp(d) => d.set_sample_rate(sr),
            BuiltinDsp::Rodharerist(d) => d.set_sample_rate(sr),
            BuiltinDsp::SpherePlugin {
                plugin_id,
                params,
                state,
            } => state.rebuild(plugin_id, params, sr as u32),
            BuiltinDsp::Wrapsynth { dsp, .. } => dsp.set_sample_rate(sr),
            BuiltinDsp::Meowsyn(d) => d.set_sample_rate(sr),
            BuiltinDsp::Drumsampler(d) => d.set_sample_rate(sr),
            BuiltinDsp::Quicksampler { dsp, .. } => dsp.choke(),
            BuiltinDsp::Slicer { dsp, .. } => dsp.all_notes_off(),
            BuiltinDsp::Soundfont { .. } => {}
        }
    }

    pub fn set_tempo_bpm(&mut self, bpm: f32) {
        if let BuiltinDsp::Wrapsynth { dsp, .. } = &mut self.dsp {
            dsp.set_tempo_bpm(bpm.clamp(20.0, 300.0));
        }
    }

    pub fn reset(&mut self) {
        match &mut self.dsp {
            BuiltinDsp::Equz8(d) => d.reset(),
            BuiltinDsp::Equzx(d) => d.reset(),
            BuiltinDsp::Compresser(d) => d.reset(),
            BuiltinDsp::Fa2a(d) => d.reset(),
            BuiltinDsp::Fa76(d) => d.reset(),
            BuiltinDsp::Zcomp(d) => d.reset(),
            BuiltinDsp::Echospace(d) => d.reset(),
            BuiltinDsp::Verbspace(d) => d.reset(),
            BuiltinDsp::Imager(d) => d.reset(),
            BuiltinDsp::Burnlimit(d) => d.reset(),
            BuiltinDsp::Clipper67(d) => d.reset(),
            BuiltinDsp::Transient(d) => d.reset(),
            BuiltinDsp::Waygate(d) => d.reset(),
            BuiltinDsp::Drumsilencer(d) => d.reset(),
            BuiltinDsp::C1073(d) => d.reset(),
            BuiltinDsp::Mixstation(d) => d.reset(),
            BuiltinDsp::Whitesharp(d) => d.reset(),
            BuiltinDsp::Rodharerist(d) => d.reset(),
            BuiltinDsp::SpherePlugin {
                plugin_id,
                params,
                state,
            } => state.rebuild(plugin_id, params, self.sample_rate as u32),
            BuiltinDsp::Wrapsynth { dsp, .. } => dsp.reset(),
            BuiltinDsp::Meowsyn(d) => d.reset(),
            BuiltinDsp::Drumsampler(d) => d.reset(),
            BuiltinDsp::Quicksampler { dsp, .. } => dsp.all_notes_off(),
            BuiltinDsp::Slicer { dsp, .. } => dsp.all_notes_off(),
            BuiltinDsp::Soundfont { player, .. } => player.reset(),
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: f32) {
        let vel01 = if velocity > 1.0 {
            (velocity / 127.0).clamp(0.0, 1.0)
        } else {
            velocity.clamp(0.0, 1.0)
        };
        let vel127 = (vel01 * 127.0).round().clamp(1.0, 127.0) as u8;
        match &mut self.dsp {
            BuiltinDsp::Wrapsynth { dsp, .. } => dsp.note_on(note, vel127),
            BuiltinDsp::Meowsyn(d) => d.note_on(note, vel127),
            BuiltinDsp::Drumsampler(d) => d.note_on(note, vel127),
            BuiltinDsp::Quicksampler { dsp, .. } => dsp.note_on(note, vel127),
            BuiltinDsp::Slicer { dsp, .. } => dsp.note_on(note, vel127),
            BuiltinDsp::Soundfont {
                player, transpose, ..
            } => {
                let key = (note as i32 + *transpose).clamp(0, 127) as u8;
                let _ = player.note_on(0, key, vel127);
            }
            _ => {}
        }
    }

    pub fn note_off(&mut self, note: u8) {
        match &mut self.dsp {
            BuiltinDsp::Wrapsynth { dsp, .. } => dsp.note_off(note),
            BuiltinDsp::Meowsyn(d) => d.note_off(note),
            BuiltinDsp::Drumsampler(d) => d.note_off(note),
            BuiltinDsp::Quicksampler { dsp, .. } => dsp.note_off(note),
            BuiltinDsp::Slicer { dsp, .. } => dsp.note_off(note),
            BuiltinDsp::Soundfont {
                player, transpose, ..
            } => {
                let key = (note as i32 + *transpose).clamp(0, 127) as u8;
                let _ = player.note_off(0, key);
            }
            _ => {}
        }
    }

    pub fn all_notes_off(&mut self) {
        match &mut self.dsp {
            BuiltinDsp::Wrapsynth { dsp, .. } => {
                for n in 0..=127 {
                    dsp.note_off(n);
                }
            }
            BuiltinDsp::Meowsyn(d) => {
                for n in 0..=127 {
                    d.note_off(n);
                }
            }
            BuiltinDsp::Drumsampler(d) => {
                d.all_notes_off();
            }
            BuiltinDsp::Quicksampler { dsp, .. } => dsp.all_notes_off(),
            BuiltinDsp::Slicer { dsp, .. } => dsp.all_notes_off(),
            BuiltinDsp::Soundfont { player, .. } => player.all_notes_off(false),
            _ => {}
        }
    }

    pub fn set_param(&mut self, param_id: &str, raw_value: f32) -> bool {
        let mut value = if raw_value.is_finite() { raw_value } else { 0.0 };
        if let Some(spec) = self.param_specs.iter_mut().find(|s| s.id == param_id) {
            value = value.clamp(spec.min, spec.max);
            spec.value = value;
        }
        self.param_values.insert(param_id.to_string(), value);

        match &mut self.dsp {
            BuiltinDsp::Equz8(d) => {
                if let Some(idx) = equz8::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Equzx(d) => {
                if let Some(idx) = equzx::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Compresser(d) => {
                if let Some(idx) = compresser::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Fa2a(d) => {
                if let Some(idx) = fa2a::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Fa76(d) => {
                if let Some(idx) = fa76::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Zcomp(d) => {
                if let Some(idx) = zcomp::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Echospace(d) => {
                if let Some(idx) = echospace::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Verbspace(d) => {
                if let Some(idx) = verbspace::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Imager(d) => {
                if let Some(idx) = imager::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Burnlimit(d) => {
                if let Some(idx) = burnlimit::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Clipper67(d) => {
                if let Some(idx) = clipper67::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Transient(d) => {
                if let Some(idx) = transient::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Waygate(d) => {
                if let Some(idx) = waygate::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Drumsilencer(d) => {
                if let Some(idx) = drumsilencer::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::C1073(d) => {
                let mut p = d.params().clone();
                match param_id {
                    "power" => p.power = value >= 0.5,
                    "input_gain_db" | "preamp_db" => p.preamp_db = value,
                    "high_gain_db" | "high_shelf_db" => p.high_shelf_db = value,
                    "mid_freq_hz" | "mid_hz" => p.mid_hz = value,
                    "mid_gain_db" | "mid_db" => p.mid_db = value,
                    "low_freq_hz" | "low_shelf_hz" => p.low_shelf_hz = value,
                    "low_gain_db" | "low_shelf_db" => p.low_shelf_db = value,
                    "hp_enabled" | "highpass_enabled" => p.highpass_enabled = value >= 0.5,
                    "hp_freq_hz" | "highpass_hz" => p.highpass_hz = value,
                    "phase_invert" | "drive" => p.drive = value,
                    "output_trim_db" | "output_db" => p.output_db = value,
                    _ => return false,
                }
                d.set_params(p);
                return true;
            }
            BuiltinDsp::Mixstation(d) => {
                if let Some(idx) = mixstation::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Whitesharp(d) => {
                if let Some(idx) = whitesharp::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Rodharerist(d) => {
                return d.apply_ui_param(param_id, value);
            }
            BuiltinDsp::SpherePlugin {
                plugin_id,
                params,
                state,
            } => {
                params.insert(param_id.to_string(), serde_json::json!(value));
                state.rebuild(plugin_id, params, self.sample_rate as u32);
                return true;
            }
            BuiltinDsp::Wrapsynth { dsp, .. } => {
                if let Some(idx) = wrapsynth::ipc::ui_param_index(param_id) {
                    dsp.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Meowsyn(d) => {
                let mut p = d.params().clone();
                match param_id {
                    "power" => p.power = value >= 0.5,
                    "waveform" => {
                        p.shape = match value.round() as i32 {
                            1 => meowsyn::OscShape::Square,
                            2 => meowsyn::OscShape::Sine,
                            _ => meowsyn::OscShape::SoftSaw,
                        };
                    }
                    "attack_ms" => p.attack_ms = value,
                    "decay_ms" => p.decay_ms = value,
                    "sustain" => p.sustain = value,
                    "release_ms" => p.release_ms = value,
                    "cutoff_hz" => p.cutoff_hz = value,
                    "resonance" => p.resonance = value,
                    "env_mod" | "meow_vowel" | "chorus_mix" | "vibrato_rate_hz"
                    | "vibrato_depth" | "glide_ms" => {
                        p.detune_cents = (value * 18.0).clamp(0.0, 50.0);
                    }
                    "gain_db" => p.gain_db = value,
                    _ => return false,
                }
                d.set_params(p);
                return true;
            }
            BuiltinDsp::Drumsampler(d) => {
                if let Some(idx) = drumsampler::ipc::ui_param_index(param_id) {
                    d.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Quicksampler { dsp, .. } => {
                if let Some(idx) = quicksampler::ipc::ui_param_index(param_id) {
                    dsp.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Slicer { dsp, .. } => {
                if let Some(idx) = slicer::ipc::ui_param_index(param_id) {
                    dsp.apply_wire_param(idx, value);
                    return true;
                }
            }
            BuiltinDsp::Soundfont {
                gain_db,
                pan,
                transpose,
                ..
            } => match param_id {
                "gain_db" => {
                    *gain_db = value.clamp(-36.0, 18.0);
                    return true;
                }
                "pan" => {
                    *pan = value.clamp(-1.0, 1.0);
                    return true;
                }
                "transpose" => {
                    *transpose = value.round().clamp(-24.0, 24.0) as i32;
                    return true;
                }
                _ => return false,
            },
        }
        false
    }

    pub fn load_sample_file(&mut self, path: &Path, pad_index: Option<usize>) -> Result<(), String> {
        match &mut self.dsp {
            BuiltinDsp::Drumsampler(d) => {
                let idx = pad_index.unwrap_or(0).min(63);
                let decoded = audio_io::decode_audio_file_stereo(path, self.sample_rate as u32)?;
                let interleaved = decoded.interleaved();
                if let Some(loader) = d.pad_loader(idx) {
                    loader.submit(Box::new(drumsampler::PadBuffer {
                        samples: interleaved.into_boxed_slice(),
                        channels: 2,
                        frames: decoded.frames,
                        sample_rate: decoded.sample_rate as f32,
                    }));
                }
                Ok(())
            }
            BuiltinDsp::Quicksampler { dsp, .. } => {
                let decoded = audio_io::decode_audio_file_stereo(path, self.sample_rate as u32)?;
                let interleaved = decoded.interleaved();
                let sample = std::sync::Arc::new(
                    quicksampler::SampleData::from_interleaved(
                        &interleaved,
                        2,
                        decoded.sample_rate,
                    )
                    .ok_or_else(|| "Invalid sample data".to_string())?,
                );
                dsp.sample_loader().submit(Some(sample));
                Ok(())
            }
            BuiltinDsp::Slicer { dsp, .. } => {
                let decoded = audio_io::decode_audio_file_stereo(path, self.sample_rate as u32)?;
                let interleaved = decoded.interleaved();
                let sample = std::sync::Arc::new(
                    quicksampler::SampleData::from_interleaved(
                        &interleaved,
                        2,
                        decoded.sample_rate,
                    )
                    .ok_or_else(|| "Invalid sample data".to_string())?,
                );
                dsp.sample_loader().submit(Some(sample));
                Ok(())
            }
            BuiltinDsp::Soundfont { player, .. } => {
                let settings = SoundfontPlayerSettings {
                    sample_rate: self.sample_rate as i32,
                    ..SoundfontPlayerSettings::default()
                };
                let new_player = SoundfontPlayer::from_path(path, settings)
                    .map_err(|e| format!("SoundFont load failed: {e}"))?;
                **player = new_player;
                Ok(())
            }
            _ => Err(format!("Plugin '{}' does not load sample files", self.plugin_id)),
        }
    }

    pub fn apply_preset(&mut self, preset_name: &str) -> bool {
        let presets = presets_for(&self.plugin_id);
        let Some((_, kvs)) = presets.iter().find(|(n, _)| n.eq_ignore_ascii_case(preset_name)) else {
            return false;
        };
        for (k, v) in kvs {
            self.set_param(k, *v);
        }
        self.active_preset = preset_name.to_string();
        true
    }

    /// Process a stereo buffer in-place (`left` and `right` of length `frames`).
    /// For instruments, synthesizes into `left` and `right` (adding to existing content).
    /// For effects, transforms `left` and `right` in-place.
    pub fn process_stereo_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }
        if !self.enabled {
            return;
        }

        let mut in_peak = 0.0f32;
        for i in 0..frames {
            in_peak = in_peak.max(left[i].abs()).max(right[i].abs());
        }

        match &mut self.dsp {
            BuiltinDsp::Equz8(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Equzx(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Compresser(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Fa2a(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Fa76(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Zcomp(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Echospace(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Verbspace(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Imager(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Burnlimit(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Clipper67(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Transient(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Waygate(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Drumsilencer(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::C1073(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Mixstation(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Whitesharp(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::Rodharerist(d) => process_effect_loop(d.as_mut(), left, right, frames),
            BuiltinDsp::SpherePlugin {
                plugin_id,
                params,
                state,
            } => {
                for i in 0..frames {
                    let (ol, or) = sphere_audio_plugins::process_stereo_sample(
                        plugin_id,
                        true,
                        params,
                        state,
                        left[i],
                        right[i],
                    );
                    left[i] = ol;
                    right[i] = or;
                }
            }
            BuiltinDsp::Wrapsynth { dsp, scratch } => {
                if scratch.len() < frames * 2 {
                    scratch.resize(frames * 2, 0.0);
                }
                scratch[..frames * 2].fill(0.0);
                dsp.process_block(&mut scratch[..frames * 2], frames);
                for i in 0..frames {
                    left[i] += scratch[i * 2];
                    right[i] += scratch[i * 2 + 1];
                }
            }
            BuiltinDsp::Meowsyn(d) => {
                for i in 0..frames {
                    let (ol, or) = d.process_stereo();
                    left[i] += ol;
                    right[i] += or;
                }
            }
            BuiltinDsp::Drumsampler(d) => {
                for i in 0..frames {
                    let (ol, or) = d.process_stereo();
                    left[i] += ol;
                    right[i] += or;
                }
            }
            BuiltinDsp::Quicksampler { dsp, scratch } => {
                if scratch.len() < frames * 2 {
                    scratch.resize(frames * 2, 0.0);
                }
                scratch[..frames * 2].fill(0.0);
                dsp.process_block(&mut scratch[..frames * 2], frames);
                for i in 0..frames {
                    left[i] += scratch[i * 2];
                    right[i] += scratch[i * 2 + 1];
                }
            }
            BuiltinDsp::Slicer { dsp, scratch } => {
                if scratch.len() < frames * 2 {
                    scratch.resize(frames * 2, 0.0);
                }
                scratch[..frames * 2].fill(0.0);
                dsp.process_block(&mut scratch[..frames * 2], frames);
                for i in 0..frames {
                    left[i] += scratch[i * 2];
                    right[i] += scratch[i * 2 + 1];
                }
            }
            BuiltinDsp::Soundfont {
                player,
                left_buf,
                right_buf,
                gain_db,
                pan,
                ..
            } => {
                if left_buf.len() < frames {
                    left_buf.resize(frames, 0.0);
                    right_buf.resize(frames, 0.0);
                }
                left_buf[..frames].fill(0.0);
                right_buf[..frames].fill(0.0);
                let _ = player.render(&mut left_buf[..frames], &mut right_buf[..frames]);
                let g = 10.0f32.powf(*gain_db / 20.0);
                let angle = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
                let gl = angle.cos() * g;
                let gr = angle.sin() * g;
                for i in 0..frames {
                    left[i] += left_buf[i] * gl;
                    right[i] += right_buf[i] * gr;
                }
            }
        }

        let mut out_peak = 0.0f32;
        for i in 0..frames {
            out_peak = out_peak.max(left[i].abs()).max(right[i].abs());
        }
        self.last_in_peak = in_peak;
        self.last_out_peak = out_peak;
        if in_peak > 1.0e-5 && out_peak < in_peak {
            self.last_gr_db = 20.0 * (in_peak / out_peak.max(1.0e-5)).log10();
        } else {
            self.last_gr_db *= 0.85;
        }
    }

    pub fn meter_snapshot(&self) -> PluginMeterSnapshot {
        let in_db = if self.last_in_peak > 1.0e-5 {
            20.0 * self.last_in_peak.log10()
        } else {
            -96.0
        };
        let out_db = if self.last_out_peak > 1.0e-5 {
            20.0 * self.last_out_peak.log10()
        } else {
            -96.0
        };
        let pad_levels = if let BuiltinDsp::Drumsampler(d) = &self.dsp {
            d.pad_levels()[..16].to_vec()
        } else {
            Vec::new()
        };
        let eq_curve_db = self.compute_eq_curve(64);
        PluginMeterSnapshot {
            input_peak_db: in_db.clamp(-96.0, 12.0),
            output_peak_db: out_db.clamp(-96.0, 12.0),
            gain_reduction_db: self.last_gr_db.clamp(0.0, 36.0),
            pad_levels,
            eq_curve_db,
        }
    }

    /// Evaluates the composite EQ magnitude response (in dB) across `points` log-spaced
    /// frequencies between 20 Hz and 20 kHz for EQ plugins (`equz8`, `equzx`, `sphere.eq8`, `c1073`).
    pub fn compute_eq_curve(&self, points: usize) -> Vec<f32> {
        let n = points.clamp(16, 256);
        let mut curve = vec![0.0f32; n];
        let is_eq = matches!(
            self.plugin_id.as_str(),
            "equz8" | "equzx" | "sphere.eq8" | "c1073"
        );
        if !is_eq {
            return Vec::new();
        }

        let min_f = 20.0f32.ln();
        let max_f = 20_000.0f32.ln();
        for (idx, slot) in curve.iter_mut().enumerate() {
            let t = idx as f32 / (n.saturating_sub(1).max(1) as f32);
            let f = (min_f + t * (max_f - min_f)).exp();
            let mut db = 0.0f32;

            if self.plugin_id == "c1073" {
                let low_f = *self.param_values.get("low_freq_hz").unwrap_or(&110.0);
                let low_g = *self.param_values.get("low_gain_db").unwrap_or(&0.0);
                let mid_f = *self.param_values.get("mid_freq_hz").unwrap_or(&1600.0);
                let mid_g = *self.param_values.get("mid_gain_db").unwrap_or(&0.0);
                let high_g = *self.param_values.get("high_gain_db").unwrap_or(&0.0);
                db += bell_or_shelf_response_db(f, low_f, low_g, 0.7, 1);
                db += bell_or_shelf_response_db(f, mid_f, mid_g, 1.2, 0);
                db += bell_or_shelf_response_db(f, 12_000.0, high_g, 0.7, 2);
            } else {
                for b in 1..=8 {
                    let en = *self
                        .param_values
                        .get(&format!("band{b}_enabled"))
                        .or_else(|| self.param_values.get(&format!("band{b}Active")))
                        .unwrap_or(&1.0);
                    if en < 0.5 {
                        continue;
                    }
                    let bf = *self
                        .param_values
                        .get(&format!("band{b}_freq_hz"))
                        .or_else(|| self.param_values.get(&format!("band{b}Freq")))
                        .unwrap_or(&1000.0);
                    let bg = *self
                        .param_values
                        .get(&format!("band{b}_gain_db"))
                        .or_else(|| self.param_values.get(&format!("band{b}Gain")))
                        .unwrap_or(&0.0);
                    let bq = *self
                        .param_values
                        .get(&format!("band{b}_q"))
                        .or_else(|| self.param_values.get(&format!("band{b}Q")))
                        .unwrap_or(&1.0);
                    let bmode = *self
                        .param_values
                        .get(&format!("band{b}_mode"))
                        .unwrap_or(&(if b == 1 {
                            1.0
                        } else if b == 8 {
                            2.0
                        } else {
                            0.0
                        })) as i32;
                    db += bell_or_shelf_response_db(f, bf, bg, bq, bmode);
                }
            }
            *slot = db.clamp(-24.0, 24.0);
        }
        curve
    }
}

#[inline]
fn process_effect_loop<E: StereoEffect + ?Sized>(
    effect: &mut E,
    left: &mut [f32],
    right: &mut [f32],
    frames: usize,
) {
    for i in 0..frames {
        let (ol, or) = effect.process_stereo(left[i], right[i]);
        left[i] = if ol.is_finite() { ol } else { 0.0 };
        right[i] = if or.is_finite() { or } else { 0.0 };
    }
}

fn bell_or_shelf_response_db(freq: f32, center_freq: f32, gain_db: f32, q: f32, mode: i32) -> f32 {
    let ratio = (freq.max(10.0) / center_freq.max(10.0)).ln();
    let width = (1.0 / q.clamp(0.2, 12.0)).max(0.08);
    match mode {
        1 => {
            // Low shelf
            let s = 1.0 / (1.0 + ((freq / center_freq.max(20.0)).powf(2.0 * q.clamp(0.4, 4.0))));
            gain_db * s
        }
        2 => {
            // High shelf
            let s = 1.0 / (1.0 + ((center_freq.max(20.0) / freq).powf(2.0 * q.clamp(0.4, 4.0))));
            gain_db * s
        }
        3 => {
            // High-pass
            if freq < center_freq {
                -24.0 * ((center_freq / freq.max(10.0)).log2().clamp(0.0, 2.0))
            } else {
                0.0
            }
        }
        4 => {
            // Low-pass
            if freq > center_freq {
                -24.0 * ((freq / center_freq.max(20.0)).log2().clamp(0.0, 2.0))
            } else {
                0.0
            }
        }
        _ => {
            // Bell
            gain_db * (-0.5 * (ratio / width).powi(2)).exp()
        }
    }
}

pub fn canonicalize_id(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "eq" | "eq-z8" | "equz8" => "equz8".to_string(),
        "eq-zx" | "equzx" => "equzx".to_string(),
        "comp" | "compressor" | "compresser" => "compresser".to_string(),
        "la2a" | "fa-2a" | "fa2a" => "fa2a".to_string(),
        "1176" | "fa-76" | "fa76" => "fa76".to_string(),
        "z-comp" | "zcomp" => "zcomp".to_string(),
        "delay" | "echospace" => "echospace".to_string(),
        "reverb" | "verbspace" => "verbspace".to_string(),
        "stereo" | "imager" => "imager".to_string(),
        "limiter" | "burnlimit" => "burnlimit".to_string(),
        "clipper" | "clipper67" | "67clipper" => "67clipper".to_string(),
        "transient" => "transient".to_string(),
        "gate" | "waygate" => "waygate".to_string(),
        "drumsilencer" => "drumsilencer".to_string(),
        "1073" | "c-1073" | "c1073" => "c1073".to_string(),
        "mixstation" => "mixstation".to_string(),
        "whitesharp" => "whitesharp".to_string(),
        "amp" | "rodharerist" | "rodhareist" => "rodharerist".to_string(),
        "synth" | "wrapsynth" => "wrapsynth".to_string(),
        "meowsyn" => "meowsyn".to_string(),
        "drums" | "drumsampler" => "drumsampler".to_string(),
        "sampler" | "quicksampler" => "quicksampler".to_string(),
        "slicer" => "slicer".to_string(),
        "sf2" | "soundfont" | "sphere-soundfont-player" => "soundfont".to_string(),
        other => other.to_string(),
    }
}

pub fn plugin_catalog() -> Vec<PluginCatalogEntry> {
    let items: &[(&str, &str, &str, PluginKind, &str)] = &[
        // Instruments
        (
            "wrapsynth",
            "WrapSynth",
            "Synthesizer",
            PluginKind::Instrument,
            "Dual-oscillator wavetable & virtual-analog synthesizer with unison, sub, filter envelope & FX",
        ),
        (
            "meowsyn",
            "MeowSyn",
            "Synthesizer",
            PluginKind::Instrument,
            "Expressive polyphonic formant & vowel lead synthesizer with chorus and vibrato",
        ),
        (
            "drumsampler",
            "Drum Sampler",
            "Drums & Percussion",
            PluginKind::Instrument,
            "64-pad MPC drum sampler with per-pad pitch, ADSR envelope, filter, pan and choke groups",
        ),
        (
            "quicksampler",
            "QuickSampler",
            "Sampler",
            PluginKind::Instrument,
            "Chromatic multi-mode sampler with loop crossfade, pitch tracking and resonant filter",
        ),
        (
            "slicer",
            "Slicer",
            "Beat Chopper",
            PluginKind::Instrument,
            "Transient beat slicer mapping loop slices across MIDI notes with per-slice pitch & reverse",
        ),
        (
            "soundfont",
            "SoundFont GM Player",
            "Sampled Instruments",
            PluginKind::Instrument,
            "Polyphonic SoundFont (.sf2) instrument engine with embedded studio bank",
        ),
        // Effects — EQ & Console
        (
            "equz8",
            "EQ-Z8",
            "EQ",
            PluginKind::Effect,
            "8-band surgical parametric equalizer with Mid/Side routing, auto-gain and real-time analyzer",
        ),
        (
            "equzx",
            "EQ-ZX Mastering",
            "EQ",
            PluginKind::Effect,
            "Mastering-grade hybrid linear/minimum phase parametric equalizer",
        ),
        (
            "c1073",
            "C-1073 Preamp/EQ",
            "Console & Character",
            PluginKind::Effect,
            "Classic British Class-A transformer mic preamp and inductor 3-band console EQ",
        ),
        (
            "mixstation",
            "MixStation Strip",
            "Console & Character",
            PluginKind::Effect,
            "Full SSL/Neve style channel strip with filters, 4-band EQ, compressor, gate and drive",
        ),
        // Effects — Dynamics
        (
            "compresser",
            "Compresser",
            "Dynamics",
            PluginKind::Effect,
            "Precision studio bus & track compressor with sidechain HPF, knee control and parallel mix",
        ),
        (
            "fa2a",
            "FA-2A Optical",
            "Dynamics",
            PluginKind::Effect,
            "Warm program-dependent electro-optical tube leveling amplifier",
        ),
        (
            "fa76",
            "FA-76 FET",
            "Dynamics",
            PluginKind::Effect,
            "Ultra-fast FET limiting amplifier with all-buttons-in ratio mode",
        ),
        (
            "zcomp",
            "Z-Comp VCA",
            "Dynamics",
            PluginKind::Effect,
            "Punchy VCA master bus glue compressor with character modes",
        ),
        (
            "transient",
            "Transient Shaper",
            "Dynamics",
            PluginKind::Effect,
            "Attack and sustain envelope sculptor for drums, bass and acoustic instruments",
        ),
        (
            "waygate",
            "WayGate",
            "Dynamics",
            PluginKind::Effect,
            "Hysteresis noise gate with sidechain filter and hold/release shaping",
        ),
        (
            "drumsilencer",
            "DrumSilencer",
            "Dynamics",
            PluginKind::Effect,
            "Zero-latency close-mic drum bleed suppressor and transient gate",
        ),
        // Effects — Spatial & Delay/Reverb
        (
            "verbspace",
            "VerbSpace",
            "Reverb & Delay",
            PluginKind::Effect,
            "Lush algorithmic hall, plate & shimmer studio reverb with modulation and ducking",
        ),
        (
            "echospace",
            "EchoSpace",
            "Reverb & Delay",
            PluginKind::Effect,
            "Multi-tap stereo tape & ping-pong echo with wow/flutter and feedback filtering",
        ),
        (
            "imager",
            "Imager",
            "Spatial & Stereo",
            PluginKind::Effect,
            "Multi-band stereo width imager, mono-maker bass crossover and Haas enhancer",
        ),
        // Effects — Mastering, Saturation & Amp
        (
            "burnlimit",
            "BurnLimit",
            "Mastering",
            PluginKind::Effect,
            "True-peak brickwall mastering limiter with ceiling, lookahead and saturation",
        ),
        (
            "67clipper",
            "67Clipper",
            "Mastering",
            PluginKind::Effect,
            "Variable knee soft-to-hard peak clipper with oversampled harmonic shaping",
        ),
        (
            "whitesharp",
            "WhiteSharp Overdrive",
            "Distortion & Amp",
            PluginKind::Effect,
            "Asymmetric tube screamer & harmonic saturation pedal processor",
        ),
        (
            "rodharerist",
            "Rodhareist Amp Suite",
            "Distortion & Amp",
            PluginKind::Effect,
            "Complete guitar/bass amp head, noise gate, overdrive, cabinet IR and room suite",
        ),
        (
            "sphere.eq8",
            "Sphere EQ8",
            "Sphere Core",
            PluginKind::Effect,
            "Lightweight zero-latency 8-band biquad insert EQ",
        ),
        (
            "sphere.drive",
            "Sphere Drive",
            "Sphere Core",
            PluginKind::Effect,
            "Lightweight zero-latency soft-tanh harmonic saturator",
        ),
        (
            "sphere.comp",
            "Sphere Compressor",
            "Sphere Core",
            PluginKind::Effect,
            "Lightweight zero-latency soft-knee track compressor",
        ),
    ];

    items
        .iter()
        .map(|(id, name, cat, kind, desc)| {
            let presets = presets_for(id)
                .into_iter()
                .map(|(p, _)| p.to_string())
                .collect();
            PluginCatalogEntry {
                id: (*id).to_string(),
                name: (*name).to_string(),
                category: (*cat).to_string(),
                kind: *kind,
                description: (*desc).to_string(),
                presets,
            }
        })
        .collect()
}

fn spec(
    id: &str,
    name: &str,
    default_value: f32,
    min: f32,
    max: f32,
    unit: &str,
    group: &str,
) -> PluginParamSpec {
    PluginParamSpec {
        id: id.to_string(),
        name: name.to_string(),
        value: default_value,
        default_value,
        min,
        max,
        unit: unit.to_string(),
        group: group.to_string(),
    }
}

pub fn build_param_specs_for(plugin_id: &str) -> Vec<PluginParamSpec> {
    match plugin_id {
        "equz8" | "equzx" => {
            let mut out = vec![
                spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
                spec("output_gain_db", "Output Gain", 0.0, -24.0, 24.0, "dB", "Global"),
                spec("auto_gain", "Auto Gain", 0.0, 0.0, 1.0, "bool", "Global"),
                spec("gain_scale_pct", "Gain Scale", 100.0, 10.0, 200.0, "%", "Global"),
            ];
            let default_freqs = [60.0, 125.0, 280.0, 650.0, 1500.0, 3500.0, 8000.0, 14000.0];
            for b in 1..=8 {
                let grp = format!("Band {b}");
                let def_mode = if b == 1 {
                    1.0
                } else if b == 8 {
                    2.0
                } else {
                    0.0
                };
                out.push(spec(
                    &format!("band{b}_enabled"),
                    &format!("B{b} On"),
                    1.0,
                    0.0,
                    1.0,
                    "bool",
                    &grp,
                ));
                out.push(spec(
                    &format!("band{b}_mode"),
                    &format!("B{b} Shape"),
                    def_mode,
                    0.0,
                    5.0,
                    "mode",
                    &grp,
                ));
                out.push(spec(
                    &format!("band{b}_freq_hz"),
                    &format!("B{b} Freq"),
                    default_freqs[b - 1],
                    20.0,
                    20000.0,
                    "Hz",
                    &grp,
                ));
                out.push(spec(
                    &format!("band{b}_gain_db"),
                    &format!("B{b} Gain"),
                    0.0,
                    -18.0,
                    18.0,
                    "dB",
                    &grp,
                ));
                out.push(spec(
                    &format!("band{b}_q"),
                    &format!("B{b} Q"),
                    1.0,
                    0.1,
                    12.0,
                    "Q",
                    &grp,
                ));
            }
            out
        }
        "compresser" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("threshold_db", "Threshold", -18.0, -60.0, 0.0, "dB", "Dynamics"),
            spec("ratio", "Ratio", 4.0, 1.0, 20.0, ":1", "Dynamics"),
            spec("attack_ms", "Attack", 12.0, 0.1, 200.0, "ms", "Envelope"),
            spec("release_ms", "Release", 120.0, 5.0, 2000.0, "ms", "Envelope"),
            spec("knee_db", "Knee", 6.0, 0.0, 24.0, "dB", "Dynamics"),
            spec("makeup_db", "Makeup Gain", 2.0, -12.0, 24.0, "dB", "Output"),
            spec("sc_hpf_hz", "SC HPF", 80.0, 20.0, 500.0, "Hz", "Sidechain"),
            spec("mix_pct", "Mix", 100.0, 0.0, 100.0, "%", "Output"),
        ],
        "fa2a" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("peak_reduction", "Peak Reduction", 42.0, 0.0, 100.0, "%", "Optical"),
            spec("gain", "Make-Up Gain", 36.0, 0.0, 100.0, "%", "Optical"),
            spec("limit_mode", "Compress / Limit", 0.0, 0.0, 1.0, "mode", "Optical"),
            spec("emphasis", "HF Emphasis", 25.0, 0.0, 100.0, "%", "Sidechain"),
            spec("mix", "Dry/Wet Mix", 100.0, 0.0, 100.0, "%", "Output"),
        ],
        "fa76" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("input_db", "Input Drive", 0.0, -24.0, 24.0, "dB", "FET"),
            spec("output_db", "Output Level", 0.0, -24.0, 24.0, "dB", "FET"),
            spec("attack", "Attack (Fast 7)", 5.0, 1.0, 7.0, "step", "Envelope"),
            spec("release", "Release (Fast 7)", 4.0, 1.0, 7.0, "step", "Envelope"),
            spec("ratio_mode", "Ratio (4/8/12/20/All)", 1.0, 0.0, 4.0, "mode", "FET"),
            spec("mix_pct", "Parallel Mix", 100.0, 0.0, 100.0, "%", "Output"),
        ],
        "zcomp" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("threshold_db", "Threshold", -16.0, -48.0, 0.0, "dB", "VCA"),
            spec("ratio", "Ratio", 4.0, 1.5, 12.0, ":1", "VCA"),
            spec("attack_ms", "Attack", 10.0, 0.3, 100.0, "ms", "Envelope"),
            spec("release_ms", "Release", 150.0, 20.0, 1500.0, "ms", "Envelope"),
            spec("makeup_db", "Makeup", 2.0, -12.0, 24.0, "dB", "Output"),
            spec("mix_pct", "Mix", 100.0, 0.0, 100.0, "%", "Output"),
        ],
        "verbspace" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("size_pct", "Room Size", 68.0, 5.0, 100.0, "%", "Tank"),
            spec("decay_s", "Decay Time", 2.4, 0.2, 18.0, "s", "Tank"),
            spec("predelay_ms", "Pre-Delay", 18.0, 0.0, 200.0, "ms", "Tank"),
            spec("damping_pct", "HF Damping", 42.0, 0.0, 100.0, "%", "Tone"),
            spec("diffusion_pct", "Diffusion", 78.0, 0.0, 100.0, "%", "Tank"),
            spec("mod_depth_pct", "Mod Depth", 25.0, 0.0, 100.0, "%", "Mod"),
            spec("low_cut_hz", "Low Cut", 90.0, 20.0, 600.0, "Hz", "Tone"),
            spec("high_cut_hz", "High Cut", 9500.0, 1500.0, 20000.0, "Hz", "Tone"),
            spec("mix_pct", "Dry/Wet Mix", 28.0, 0.0, 100.0, "%", "Output"),
        ],
        "echospace" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("time_ms", "Delay Time", 375.0, 10.0, 2000.0, "ms", "Delay"),
            spec("feedback_pct", "Feedback", 38.0, 0.0, 98.0, "%", "Delay"),
            spec("ping_pong", "Ping-Pong", 1.0, 0.0, 1.0, "bool", "Stereo"),
            spec("flutter_pct", "Tape Flutter", 22.0, 0.0, 100.0, "%", "Tape"),
            spec("low_cut_hz", "Low Cut", 140.0, 20.0, 800.0, "Hz", "Filter"),
            spec("high_cut_hz", "High Cut", 5200.0, 800.0, 18000.0, "Hz", "Filter"),
            spec("mix_pct", "Dry/Wet Mix", 26.0, 0.0, 100.0, "%", "Output"),
        ],
        "imager" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("width_pct", "Stereo Width", 130.0, 0.0, 200.0, "%", "Field"),
            spec("mono_bass_hz", "Mono Bass Below", 110.0, 20.0, 400.0, "Hz", "Field"),
            spec("haas_ms", "Haas Offset", 0.0, 0.0, 20.0, "ms", "Field"),
            spec("pan", "Balance", 0.0, -1.0, 1.0, "pan", "Output"),
            spec("output_db", "Output Trim", 0.0, -18.0, 18.0, "dB", "Output"),
        ],
        "burnlimit" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("gain_db", "Input Push", 3.5, -12.0, 24.0, "dB", "Limiter"),
            spec("ceiling_db", "Ceiling", -0.3, -12.0, 0.0, "dB", "Limiter"),
            spec("release_ms", "Release", 65.0, 5.0, 500.0, "ms", "Limiter"),
            spec("character_pct", "Warmth/Burn", 20.0, 0.0, 100.0, "%", "Limiter"),
        ],
        "67clipper" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("drive_db", "Input Drive", 4.0, -12.0, 24.0, "dB", "Clipper"),
            spec("ceiling_db", "Ceiling", -0.5, -18.0, 0.0, "dB", "Clipper"),
            spec("softness_pct", "Knee Softness", 45.0, 0.0, 100.0, "%", "Clipper"),
            spec("mix_pct", "Mix", 100.0, 0.0, 100.0, "%", "Output"),
        ],
        "transient" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("attack_pct", "Attack", 25.0, -100.0, 100.0, "%", "Shaper"),
            spec("sustain_pct", "Sustain", -10.0, -100.0, 100.0, "%", "Shaper"),
            spec("speed_ms", "Detector Speed", 25.0, 2.0, 150.0, "ms", "Shaper"),
            spec("output_db", "Output Trim", 0.0, -18.0, 18.0, "dB", "Output"),
        ],
        "waygate" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("threshold_db", "Threshold", -38.0, -80.0, 0.0, "dB", "Gate"),
            spec("range_db", "Reduction Range", -40.0, -80.0, 0.0, "dB", "Gate"),
            spec("attack_ms", "Attack", 2.0, 0.1, 50.0, "ms", "Envelope"),
            spec("hold_ms", "Hold", 40.0, 0.0, 500.0, "ms", "Envelope"),
            spec("release_ms", "Release", 120.0, 10.0, 1000.0, "ms", "Envelope"),
        ],
        "drumsilencer" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("threshold_db", "Trigger Threshold", -28.0, -72.0, 0.0, "dB", "De-Bleed"),
            spec("decay_ms", "Hit Decay", 140.0, 15.0, 800.0, "ms", "De-Bleed"),
            spec("bleed_reduction_db", "Bleed Reduction", -36.0, -80.0, 0.0, "dB", "De-Bleed"),
            spec("hp_freq_hz", "Detector HPF", 70.0, 20.0, 600.0, "Hz", "Detector"),
        ],
        "c1073" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("input_gain_db", "Preamp Drive", 6.0, -18.0, 36.0, "dB", "Preamp"),
            spec("high_gain_db", "12kHz High Shelf", 2.0, -16.0, 16.0, "dB", "EQ"),
            spec("mid_freq_hz", "Mid Frequency", 1600.0, 360.0, 7200.0, "Hz", "EQ"),
            spec("mid_gain_db", "Mid Bell Gain", 1.5, -18.0, 18.0, "dB", "EQ"),
            spec("low_freq_hz", "Low Frequency", 110.0, 35.0, 220.0, "Hz", "EQ"),
            spec("low_gain_db", "Low Shelf Gain", 1.5, -16.0, 16.0, "dB", "EQ"),
            spec("hp_enabled", "HPF Enable", 1.0, 0.0, 1.0, "bool", "Filter"),
            spec("hp_freq_hz", "HPF Frequency", 50.0, 30.0, 300.0, "Hz", "Filter"),
            spec("phase_invert", "Phase Invert", 0.0, 0.0, 1.0, "bool", "Global"),
            spec("output_trim_db", "Output Trim", -2.0, -24.0, 24.0, "dB", "Output"),
        ],
        "mixstation" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("input_gain_db", "Input Gain", 0.0, -24.0, 24.0, "dB", "Preamp"),
            spec("drive_pct", "Console Saturation", 22.0, 0.0, 100.0, "%", "Preamp"),
            spec("hp_freq_hz", "High-Pass", 40.0, 20.0, 400.0, "Hz", "Filter"),
            spec("low_gain_db", "Low Gain", 1.0, -15.0, 15.0, "dB", "EQ"),
            spec("low_mid_gain_db", "Low-Mid Gain", 0.0, -15.0, 15.0, "dB", "EQ"),
            spec("high_mid_gain_db", "High-Mid Gain", 1.5, -15.0, 15.0, "dB", "EQ"),
            spec("high_gain_db", "High Shelf", 2.0, -15.0, 15.0, "dB", "EQ"),
            spec("comp_amount_pct", "Compressor", 30.0, 0.0, 100.0, "%", "Dynamics"),
            spec("output_gain_db", "Output Fader", 0.0, -24.0, 24.0, "dB", "Output"),
        ],
        "whitesharp" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("drive", "Overdrive", 48.0, 0.0, 100.0, "%", "Pedal"),
            spec("tone", "Tone Filter", 56.0, 0.0, 100.0, "%", "Pedal"),
            spec("level_db", "Output Level", 0.0, -24.0, 18.0, "dB", "Pedal"),
            spec("mix_pct", "Dry/Wet", 100.0, 0.0, 100.0, "%", "Output"),
        ],
        "rodharerist" => vec![
            spec("input_gain_db", "Input Gain", 0.0, -24.0, 24.0, "dB", "Input"),
            spec("gate_threshold_db", "Gate Threshold", -58.0, -80.0, -20.0, "dB", "Input"),
            spec("od_enabled", "Overdrive On", 1.0, 0.0, 1.0, "bool", "Pedal"),
            spec("od_drive", "OD Drive", 35.0, 0.0, 100.0, "%", "Pedal"),
            spec("od_tone", "OD Tone", 55.0, 0.0, 100.0, "%", "Pedal"),
            spec("amp_channel", "Amp Channel (Clean/Crunch/Lead)", 1.0, 0.0, 2.0, "ch", "Amp"),
            spec("amp_gain", "Preamp Gain", 58.0, 0.0, 100.0, "%", "Amp"),
            spec("amp_bass", "Bass", 52.0, 0.0, 100.0, "%", "Tone Stack"),
            spec("amp_mid", "Middle", 58.0, 0.0, 100.0, "%", "Tone Stack"),
            spec("amp_treble", "Treble", 62.0, 0.0, 100.0, "%", "Tone Stack"),
            spec("amp_presence", "Presence", 54.0, 0.0, 100.0, "%", "Power Amp"),
            spec("amp_master", "Master Volume", 65.0, 0.0, 100.0, "%", "Power Amp"),
            spec("cab_enabled", "Cabinet Sim", 1.0, 0.0, 1.0, "bool", "Cabinet"),
            spec("room_mix", "Room Ambience", 18.0, 0.0, 100.0, "%", "Cabinet"),
            spec("output_gain_db", "Output Trim", 0.0, -24.0, 24.0, "dB", "Output"),
        ],
        "wrapsynth" => vec![
            spec("master_gain", "Master Level", 0.78, 0.0, 1.0, "lvl", "Global"),
            spec("osc_a_wave", "Osc A Wavetable", 1.0, 0.0, 7.0, "wave", "Osc A"),
            spec("osc_a_unison", "Osc A Unison", 4.0, 1.0, 8.0, "voices", "Osc A"),
            spec("osc_a_detune", "Osc A Detune", 0.25, 0.0, 1.0, "%", "Osc A"),
            spec("osc_a_level", "Osc A Level", 0.85, 0.0, 1.0, "lvl", "Osc A"),
            spec("osc_b_enabled", "Osc B On", 1.0, 0.0, 1.0, "bool", "Osc B"),
            spec("osc_b_wave", "Osc B Wavetable", 2.0, 0.0, 7.0, "wave", "Osc B"),
            spec("osc_b_semi", "Osc B Semitone", 0.0, -24.0, 24.0, "st", "Osc B"),
            spec("osc_b_level", "Osc B Level", 0.55, 0.0, 1.0, "lvl", "Osc B"),
            spec("sub_enabled", "Sub Osc On", 1.0, 0.0, 1.0, "bool", "Sub"),
            spec("sub_level", "Sub Level", 0.45, 0.0, 1.0, "lvl", "Sub"),
            spec("filter_cutoff", "Filter Cutoff", 3200.0, 40.0, 18000.0, "Hz", "Filter"),
            spec("filter_res", "Resonance", 0.28, 0.0, 0.95, "Q", "Filter"),
            spec("filter_env_amt", "Filter Env Amt", 0.45, -1.0, 1.0, "amt", "Filter"),
            spec("env1_attack", "Amp Attack", 0.008, 0.001, 2.0, "s", "Envelope"),
            spec("env1_decay", "Amp Decay", 0.35, 0.01, 4.0, "s", "Envelope"),
            spec("env1_sustain", "Amp Sustain", 0.72, 0.0, 1.0, "lvl", "Envelope"),
            spec("env1_release", "Amp Release", 0.28, 0.01, 5.0, "s", "Envelope"),
            spec("fx_chorus_mix", "Chorus Mix", 0.25, 0.0, 1.0, "mix", "FX"),
            spec("fx_delay_mix", "Delay Mix", 0.18, 0.0, 1.0, "mix", "FX"),
            spec("fx_reverb_mix", "Reverb Mix", 0.22, 0.0, 1.0, "mix", "FX"),
        ],
        "meowsyn" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("waveform", "Waveform (Saw/Sq/Tri/Formant)", 0.0, 0.0, 3.0, "wave", "Osc"),
            spec("meow_vowel", "Vowel Formant", 0.45, 0.0, 1.0, "vowel", "Formant"),
            spec("cutoff_hz", "Filter Cutoff", 2800.0, 80.0, 16000.0, "Hz", "Filter"),
            spec("resonance", "Resonance", 0.35, 0.0, 0.95, "Q", "Filter"),
            spec("env_mod", "Filter Env Mod", 0.5, 0.0, 1.0, "amt", "Filter"),
            spec("attack_ms", "Attack", 12.0, 1.0, 1000.0, "ms", "Envelope"),
            spec("decay_ms", "Decay", 220.0, 10.0, 2000.0, "ms", "Envelope"),
            spec("sustain", "Sustain", 0.75, 0.0, 1.0, "lvl", "Envelope"),
            spec("release_ms", "Release", 240.0, 10.0, 3000.0, "ms", "Envelope"),
            spec("vibrato_rate_hz", "Vibrato Rate", 5.5, 0.5, 14.0, "Hz", "Mod"),
            spec("vibrato_depth", "Vibrato Depth", 0.15, 0.0, 1.0, "amt", "Mod"),
            spec("glide_ms", "Portamento Glide", 25.0, 0.0, 500.0, "ms", "Mod"),
            spec("chorus_mix", "Chorus Width", 0.35, 0.0, 1.0, "mix", "FX"),
            spec("gain_db", "Output Gain", 0.0, -24.0, 12.0, "dB", "Global"),
        ],
        "drumsampler" => vec![
            spec("master_gain_db", "Master Level", 0.0, -24.0, 12.0, "dB", "Global"),
            spec("selected_pad", "Active Pad", 0.0, 0.0, 15.0, "pad", "Pad"),
            spec("pad1_gain_db", "Kick Gain", 0.0, -24.0, 12.0, "dB", "Kit Mix"),
            spec("pad1_tune_st", "Kick Tune", 0.0, -12.0, 12.0, "st", "Kit Mix"),
            spec("pad2_gain_db", "Snare Gain", 0.0, -24.0, 12.0, "dB", "Kit Mix"),
            spec("pad2_tune_st", "Snare Tune", 0.0, -12.0, 12.0, "st", "Kit Mix"),
            spec("pad3_gain_db", "Closed Hat Gain", -1.5, -24.0, 12.0, "dB", "Kit Mix"),
            spec("pad4_gain_db", "Open Hat Gain", -2.0, -24.0, 12.0, "dB", "Kit Mix"),
            spec("pad5_gain_db", "Clap Gain", 0.0, -24.0, 12.0, "dB", "Kit Mix"),
            spec("pad6_gain_db", "Tom Low Gain", 0.0, -24.0, 12.0, "dB", "Kit Mix"),
            spec("pad7_gain_db", "Tom High Gain", 0.0, -24.0, 12.0, "dB", "Kit Mix"),
            spec("pad8_gain_db", "Crash Gain", -2.5, -24.0, 12.0, "dB", "Kit Mix"),
        ],
        "quicksampler" => vec![
            spec("gain_db", "Output Gain", 0.0, -24.0, 12.0, "dB", "Global"),
            spec("tune_semi", "Transpose", 0.0, -24.0, 24.0, "st", "Pitch"),
            spec("fine_cents", "Fine Tune", 0.0, -100.0, 100.0, "ct", "Pitch"),
            spec("filter_cutoff_hz", "Filter Cutoff", 12000.0, 60.0, 20000.0, "Hz", "Filter"),
            spec("filter_res", "Filter Resonance", 0.15, 0.0, 0.95, "Q", "Filter"),
            spec("attack_ms", "Amp Attack", 5.0, 0.5, 2000.0, "ms", "Envelope"),
            spec("decay_ms", "Amp Decay", 400.0, 10.0, 4000.0, "ms", "Envelope"),
            spec("sustain", "Amp Sustain", 0.85, 0.0, 1.0, "lvl", "Envelope"),
            spec("release_ms", "Amp Release", 180.0, 5.0, 4000.0, "ms", "Envelope"),
        ],
        "slicer" => vec![
            spec("gain_db", "Master Gain", 0.0, -24.0, 12.0, "dB", "Global"),
            spec("slice_count", "Slice Divisions", 8.0, 2.0, 16.0, "slices", "Slicer"),
            spec("tune_semi", "Global Pitch", 0.0, -12.0, 12.0, "st", "Pitch"),
            spec("attack_ms", "Slice Attack", 2.0, 0.5, 200.0, "ms", "Envelope"),
            spec("decay_ms", "Slice Decay", 350.0, 20.0, 2000.0, "ms", "Envelope"),
        ],
        "soundfont" => vec![
            spec("gain_db", "Output Gain", 0.0, -36.0, 18.0, "dB", "Global"),
            spec("pan", "Stereo Pan", 0.0, -1.0, 1.0, "pan", "Global"),
            spec("transpose", "Transpose", 0.0, -24.0, 24.0, "st", "Pitch"),
        ],
        "sphere.drive" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("drive", "Drive", 35.0, 0.0, 100.0, "%", "Drive"),
            spec("mix", "Mix", 100.0, 0.0, 100.0, "%", "Output"),
            spec("outputDb", "Output", 0.0, -24.0, 24.0, "dB", "Output"),
        ],
        "sphere.comp" => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("peakReduction", "Peak Reduction", 40.0, 0.0, 100.0, "%", "Dynamics"),
            spec("mix", "Mix", 100.0, 0.0, 100.0, "%", "Output"),
            spec("outputDb", "Output", 0.0, -24.0, 24.0, "dB", "Output"),
        ],
        _ => vec![
            spec("power", "Power", 1.0, 0.0, 1.0, "bool", "Global"),
            spec("mix", "Mix", 100.0, 0.0, 100.0, "%", "Global"),
            spec("outputDb", "Output", 0.0, -24.0, 24.0, "dB", "Global"),
        ],
    }
}

pub fn presets_for(plugin_id: &str) -> Vec<(&'static str, Vec<(&'static str, f32)>)> {
    match plugin_id {
        "equz8" | "equzx" => vec![
            ("Default Flat", vec![("band1_gain_db", 0.0), ("band8_gain_db", 0.0)]),
            (
                "Vocal Presence & Air",
                vec![
                    ("band1_freq_hz", 95.0),
                    ("band1_gain_db", -3.5),
                    ("band5_freq_hz", 3200.0),
                    ("band5_gain_db", 2.8),
                    ("band8_freq_hz", 12000.0),
                    ("band8_gain_db", 3.5),
                ],
            ),
            (
                "Master Smile Contour",
                vec![
                    ("band1_freq_hz", 55.0),
                    ("band1_gain_db", 2.2),
                    ("band4_freq_hz", 420.0),
                    ("band4_gain_db", -1.6),
                    ("band8_freq_hz", 13500.0),
                    ("band8_gain_db", 2.4),
                ],
            ),
        ],
        "compresser" | "zcomp" => vec![
            (
                "Mix Bus Glue 2:1",
                vec![
                    ("threshold_db", -14.0),
                    ("ratio", 2.0),
                    ("attack_ms", 30.0),
                    ("release_ms", 180.0),
                    ("makeup_db", 1.8),
                ],
            ),
            (
                "Punchy Drum Smash",
                vec![
                    ("threshold_db", -24.0),
                    ("ratio", 8.0),
                    ("attack_ms", 8.0),
                    ("release_ms", 65.0),
                    ("makeup_db", 4.5),
                    ("mix_pct", 65.0),
                ],
            ),
            (
                "Vocal Upfront Level",
                vec![
                    ("threshold_db", -20.0),
                    ("ratio", 4.0),
                    ("attack_ms", 5.0),
                    ("release_ms", 95.0),
                    ("makeup_db", 3.0),
                ],
            ),
        ],
        "verbspace" => vec![
            (
                " lush Studio Hall",
                vec![
                    ("size_pct", 78.0),
                    ("decay_s", 3.2),
                    ("predelay_ms", 24.0),
                    ("mix_pct", 26.0),
                ],
            ),
            (
                "Tight Vocal Plate",
                vec![
                    ("size_pct", 48.0),
                    ("decay_s", 1.4),
                    ("predelay_ms", 14.0),
                    ("damping_pct", 30.0),
                    ("mix_pct", 20.0),
                ],
            ),
            (
                "Ambient Shimmer Cathedral",
                vec![
                    ("size_pct", 94.0),
                    ("decay_s", 8.5),
                    ("predelay_ms", 42.0),
                    ("mod_depth_pct", 55.0),
                    ("mix_pct", 42.0),
                ],
            ),
        ],
        "wrapsynth" => vec![
            (
                "Cyberpunk Supersaw Lead",
                vec![
                    ("osc_a_wave", 1.0),
                    ("osc_a_unison", 7.0),
                    ("osc_a_detune", 0.36),
                    ("filter_cutoff", 6800.0),
                    ("fx_chorus_mix", 0.32),
                    ("fx_delay_mix", 0.25),
                ],
            ),
            (
                "Deep Sub Pluck Bass",
                vec![
                    ("osc_a_wave", 2.0),
                    ("osc_a_unison", 2.0),
                    ("sub_enabled", 1.0),
                    ("sub_level", 0.78),
                    ("filter_cutoff", 620.0),
                    ("filter_env_amt", 0.72),
                    ("env1_decay", 0.22),
                    ("env1_sustain", 0.18),
                ],
            ),
            (
                "Warm Horizon Pad",
                vec![
                    ("osc_a_wave", 3.0),
                    ("osc_a_unison", 6.0),
                    ("filter_cutoff", 2200.0),
                    ("env1_attack", 0.45),
                    ("env1_release", 1.4),
                    ("fx_reverb_mix", 0.42),
                ],
            ),
        ],
        "meowsyn" => vec![
            (
                "Sing Vowel Lead",
                vec![
                    ("waveform", 3.0),
                    ("meow_vowel", 0.55),
                    ("cutoff_hz", 3600.0),
                    ("chorus_mix", 0.4),
                ],
            ),
            (
                "Retro Chiptune Pluck",
                vec![
                    ("waveform", 1.0),
                    ("attack_ms", 4.0),
                    ("decay_ms", 160.0),
                    ("sustain", 0.2),
                    ("chorus_mix", 0.15),
                ],
            ),
        ],
        _ => vec![("Default", vec![]), ("Warm Studio", vec![]), ("High Impact", vec![])],
    }
}
