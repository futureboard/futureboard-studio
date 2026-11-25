//! The insert picker and the browser sidebar over a sample library: a mix of
//! built-ins and invented third-party plug-ins, and a samples folder with a
//! decoded loop selected.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    div, AnyWindowHandle, App, AppContext, Context, IntoElement, ParentElement, Render, Result,
    Styled, UniformListScrollHandle, Window, WindowOptions,
};

use crate::components::file_browser::{
    BrowserTab, FileBrowserEntry, FileBrowserState, FileEntryKind,
};
use crate::components::plugin_picker::{
    plugin_picker_panel, CatalogStatus, PluginPickerCallbacks, PluginPickerPrefs,
    PluginPickerScrollHandles, PluginPickerState, PluginSearchIndex,
};
use crate::components::text_input::{TextInputCallbacks, TextInputState};
use crate::components::timeline::timeline_state::TrackType;
use crate::components::{sidebar, BrowserCallbacks};
use crate::theme::Colors;
use SpherePluginHost::{PluginFormat, PluginKind, PluginScanStatus, PluginStatus, RegistryPlugin};

fn noop<T: ?Sized + 'static>() -> Arc<dyn Fn(&T, &mut Window, &mut App) + 'static> {
    Arc::new(|_, _, _| {})
}

// ── Sample plug-in library ─────────────────────────────────────────────────

/// A third-party plug-in. Vendors are invented: a preview must not advertise
/// real products.
fn third_party(
    name: &str,
    vendor: &str,
    format: PluginFormat,
    kind: PluginKind,
    category: &str,
) -> RegistryPlugin {
    let id = format!(
        "{}.{}",
        vendor.to_lowercase().replace(' ', ""),
        name.to_lowercase().replace(' ', "-")
    );
    RegistryPlugin {
        id: id.clone(),
        name: name.to_string(),
        vendor: vendor.to_string(),
        format,
        category: category.to_string(),
        is_ara: false,
        midi_output: false,
        raw_category: Some(category.to_string()),
        sub_categories: None,
        kind,
        path: PathBuf::from(format!("C:/Program Files/Common Files/VST3/{name}.vst3")),
        class_id: None,
        version: Some("2.4.1".to_string()),
        sdk_metadata_loaded: true,
        preset_path: PathBuf::new(),
        scanned_at_ms: 0,
        status: PluginStatus::PresetReady,
        scan_status: PluginScanStatus::Success,
        error_message: None,
    }
}

pub(super) fn sample_plugins() -> Vec<RegistryPlugin> {
    use PluginFormat::{Clap, Vst3};
    use PluginKind::{Effect, Instrument};
    let mut plugins = SpherePluginHost::builtin::builtin_catalog(0);
    for (name, vendor, format, kind, category) in [
        ("Tidewater EQ", "Northwind Audio", Vst3, Effect, "Fx|EQ"),
        (
            "Tidewater Comp",
            "Northwind Audio",
            Vst3,
            Effect,
            "Fx|Dynamics|Compressor",
        ),
        ("Gale Reverb", "Northwind Audio", Clap, Effect, "Fx|Reverb"),
        ("Harbor Delay", "Northwind Audio", Vst3, Effect, "Fx|Delay"),
        (
            "Kestrel Bus",
            "Kestrel DSP",
            Vst3,
            Effect,
            "Fx|Dynamics|Compressor",
        ),
        ("Kestrel Tape", "Kestrel DSP", Vst3, Effect, "Fx|Distortion"),
        (
            "Kestrel Phase",
            "Kestrel DSP",
            Clap,
            Effect,
            "Fx|Modulation",
        ),
        (
            "Lumen Pad",
            "Lumen Labs",
            Vst3,
            Instrument,
            "Instrument|Synth",
        ),
        (
            "Lumen Keys",
            "Lumen Labs",
            Vst3,
            Instrument,
            "Instrument|Piano",
        ),
        (
            "Lumen Strings",
            "Lumen Labs",
            Vst3,
            Instrument,
            "Instrument|Sampler",
        ),
        (
            "Copperline Drums",
            "Copperline",
            Vst3,
            Instrument,
            "Instrument|Drum",
        ),
        (
            "Copperline Bass",
            "Copperline",
            Clap,
            Instrument,
            "Instrument|Synth",
        ),
        ("Meridian Meter", "Meridian", Vst3, Effect, "Fx|Analyzer"),
        ("Meridian Gain", "Meridian", Vst3, Effect, "Fx|Tools"),
        ("Orbit Chorus", "Orbit Sound", Vst3, Effect, "Fx|Modulation"),
        ("Orbit Shimmer", "Orbit Sound", Clap, Effect, "Fx|Reverb"),
    ] {
        plugins.push(third_party(name, vendor, format, kind, category));
    }
    plugins
}

pub(super) fn sample_prefs(plugins: &[RegistryPlugin]) -> PluginPickerPrefs {
    let id = |name: &str| {
        plugins
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.id.clone())
            .unwrap_or_default()
    };
    let mut prefs = PluginPickerPrefs::default();
    for name in ["EQ-Z8", "Tidewater Comp", "VerbSpace"] {
        prefs.favorites.insert(id(name));
    }
    prefs.recent = [
        "EQ-Z8",
        "Kestrel Bus",
        "VerbSpace",
        "EchoSpace",
        "Clipper67",
    ]
    .into_iter()
    .map(id)
    .collect();
    prefs.window_width = 960.0;
    prefs.window_height = 640.0;
    prefs
}

fn picker_callbacks() -> PluginPickerCallbacks {
    PluginPickerCallbacks {
        on_close: noop(),
        on_select: noop(),
        on_pick: noop(),
        on_select_filter: noop(),
        on_select_kind: noop(),
        on_select_facet: noop(),
        on_toggle_favorite: noop(),
        on_retry_load: noop(),
        on_open_plugin_manager: noop(),
        on_rebuild_database: noop(),
        on_drop_plugin: noop(),
    }
}

// ── Insert picker ───────────────────────────────────────────────────────────

struct PickerScene {
    state: PluginPickerState,
    index: Arc<PluginSearchIndex>,
    prefs: PluginPickerPrefs,
    search: TextInputState,
    scroll: PluginPickerScrollHandles,
}

impl Render for PickerScene {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .child(plugin_picker_panel(
                &self.state,
                Some(self.index.clone()),
                &self.prefs,
                CatalogStatus::Ready,
                &self.search,
                true,
                TextInputCallbacks::default(),
                picker_callbacks(),
                None,
                &self.scroll,
            ))
    }
}

pub(super) fn picker_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        let plugins = sample_plugins();
        let prefs = sample_prefs(&plugins);
        let mut state = PluginPickerState::open_for("t3", "Lead Vocal", TrackType::Audio, 2, true);
        state.set_kind_tab(crate::components::plugin_picker::KindTab::Effects);
        state.highlighted_index = 3;
        cx.new(|cx| PickerScene {
            state,
            index: Arc::new(PluginSearchIndex::from_plugins(plugins)),
            prefs,
            search: TextInputState::new("pp-search", cx.focus_handle())
                .with_placeholder("Search plug-ins, vendors, categories…"),
            scroll: PluginPickerScrollHandles::default(),
        })
    })?;
    Ok(handle.into())
}

// ── Sample folder tree ─────────────────────────────────────────────────────

const SAMPLES_ROOT: &str = "D:/Futureboard/Samples";

fn entry(dir: &str, name: &str, folder: bool, size: Option<u64>) -> FileBrowserEntry {
    let path = PathBuf::from(dir).join(name);
    FileBrowserEntry {
        name: name.to_string(),
        extension: if folder {
            String::new()
        } else {
            path.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        },
        path,
        kind: if folder {
            FileEntryKind::Folder
        } else {
            FileEntryKind::File
        },
        size_bytes: size,
    }
}

/// A Samples tab with one kit folder open and a loop selected.
pub(super) fn sample_browser_state() -> FileBrowserState {
    let mut state = FileBrowserState::default();
    let root = PathBuf::from(SAMPLES_ROOT);
    state.roots.samples = Some(root.clone());
    state.roots.audio_files = Some(PathBuf::from("D:/Futureboard/Audio Files"));
    state.roots.user_library = Some(PathBuf::from("D:/Futureboard/User Library"));
    state.tab = BrowserTab::Samples;
    let drums = root.join("Drums");
    let loops = drums.join("Loops 120");
    state.apply_loaded(
        root.clone(),
        vec![
            entry(SAMPLES_ROOT, "Bass", true, None),
            entry(SAMPLES_ROOT, "Drums", true, None),
            entry(SAMPLES_ROOT, "FX", true, None),
            entry(SAMPLES_ROOT, "Keys", true, None),
            entry(SAMPLES_ROOT, "Vocals", true, None),
        ],
    );
    let d = drums.to_string_lossy().to_string();
    state.apply_loaded(
        drums.clone(),
        vec![
            entry(&d, "Loops 120", true, None),
            entry(&d, "One Shots", true, None),
            entry(&d, "Kick Deep 01.wav", false, Some(412_000)),
            entry(&d, "Snare Crack 03.wav", false, Some(288_000)),
        ],
    );
    let l = loops.to_string_lossy().to_string();
    state.apply_loaded(
        loops.clone(),
        vec![
            entry(&l, "Break Dusty 120.wav", false, Some(2_310_000)),
            entry(&l, "Break Tight 120.wav", false, Some(2_180_000)),
            entry(&l, "Groove Ride 120.flac", false, Some(1_420_000)),
            entry(&l, "Hats Shuffle 120.wav", false, Some(1_960_000)),
            entry(&l, "Perc Loop 120.wav", false, Some(1_870_000)),
            entry(&l, "Tops Loop 120.wav", false, Some(2_040_000)),
            entry(&l, "Drum Fill 120.mid", false, Some(1_200)),
        ],
    );
    state.expanded_paths.insert(root);
    state.expanded_paths.insert(drums);
    state.expanded_paths.insert(loops.clone());
    let selected = loops.join("Break Tight 120.wav");
    seed_waveform(&selected);
    state.select(selected);
    state.update_visible_nodes();
    state
}

pub(super) fn browser_callbacks() -> BrowserCallbacks {
    let action =
        || Arc::new(|_: &mut Window, _: &mut App| {}) as Arc<dyn Fn(&mut Window, &mut App)>;
    BrowserCallbacks {
        on_select_tab: noop(),
        on_select_kind: noop(),
        on_toggle: noop(),
        on_select: noop(),
        on_reveal: noop(),
        on_activate_file: noop(),
        on_audition: noop(),
        on_context_menu: noop(),
        on_collapse_all: action(),
        on_rescan: action(),
        on_toggle_preview: action(),
        on_stop_preview: action(),
    }
}

// ── Browser sidebar ────────────────────────────────────────────────────────

struct BrowserScene {
    state: FileBrowserState,
    scroll: UniformListScrollHandle,
    focus: gpui::FocusHandle,
    search: TextInputState,
}

impl Render for BrowserScene {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .child(sidebar(
                &self.state,
                self.scroll.clone(),
                &self.focus,
                &self.search,
                false,
                true,
                TextInputCallbacks::default(),
                browser_callbacks(),
                crate::i18n::I18n::new("en-US"),
                None,
            ))
    }
}

pub(super) fn browser_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        // The selected loop is auditioning, so the card shows its playhead.
        let mut state = sample_browser_state();
        let playing = state.selected.clone().expect("selection");
        state.set_preview_playing(playing);
        state.apply_preview_position(Some(1.35));
        cx.new(|cx| BrowserScene {
            state,
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            search: TextInputState::new("browser-search", cx.focus_handle())
                .with_placeholder("Search Samples"),
        })
    })?;
    Ok(handle.into())
}

/// Peaks for the selected loop: a two-bar break at 120 BPM, so the preview
/// pane has the decoded waveform a real file would give it.
fn seed_waveform(path: &std::path::Path) {
    use crate::components::timeline::waveform_cache::{
        install_ready, WaveformLod, WaveformPeak, WaveformPreview,
    };
    let sample_rate = 48_000u32;
    let seconds = 4.0f64;
    let frames = (seconds * sample_rate as f64) as u64;
    let spp = 256usize;
    let peaks = (0..(frames as usize / spp))
        .map(|i| {
            let t = (i * spp) as f64 / sample_rate as f64;
            let beat = (t * 4.0).fract(); // sixteenths at 120 BPM
            let accent = if ((t * 2.0).fract()) < 0.06 {
                1.0
            } else {
                0.55
            };
            let env = (-(beat * 9.0)).exp() * accent;
            let noise = (((i as f64) * 12.9898).sin() * 43758.5453).fract().abs();
            let a = (0.08 + env * 0.85 * (0.6 + 0.4 * noise)) as f32;
            WaveformPeak {
                min: -a,
                max: a * 0.96,
            }
        })
        .collect();
    install_ready(
        &path.to_string_lossy(),
        Arc::new(WaveformPreview {
            sample_rate,
            channels: 2,
            duration_seconds: seconds,
            total_frames: frames,
            lods: vec![WaveformLod {
                samples_per_peak: spp,
                peaks,
            }],
        }),
    );
}

// ── Spatial Room ──────────────────────────────────────────────────────────

/// The 3D Room Preview on a 7.1.4 mix: a lead on the front wall, a stereo
/// pad spread wide behind, drums kept in place inside their bus, an automated
/// FX source caught mid-move, the speakers lit by a few levels.
pub(super) fn room_scene(
    options: WindowOptions,
    cx: &mut App,
    licensed: bool,
) -> Result<AnyWindowHandle> {
    use crate::components::room_window::{RoomCallbacks, RoomSnapshot, RoomSource, RoomWindow};
    use solfege_spatialaudio::{RoomPosition, SourceParams, SpatialFormat, SpeakerLayout};
    let rgb = |hex: u32| gpui::rgb(hex);
    let place = |x: f32, y: f32, z: f32, width: f32, spread: f32| SourceParams {
        position: RoomPosition::new(x, y, z),
        width,
        spread,
        lfe: 0.0,
    };
    let source = |id: &str, name: &str, color: gpui::Rgba, heard: SourceParams| RoomSource {
        id: id.to_string(),
        name: name.to_string(),
        color,
        heard,
        base: heard,
        automated: false,
        bus: None,
    };
    let layout = SpeakerLayout::Surround714;
    let format = SpatialFormat::Surround(layout);
    let mut fx = source(
        "fx",
        "Riser FX",
        rgb(0xE8B75C),
        place(0.62, -0.35, 0.7, 0.0, 0.0),
    );
    fx.automated = true;
    fx.base = place(-0.4, 0.8, 0.2, 0.0, 0.0);
    let mut kick = source(
        "kick",
        "Kick",
        rgb(0xE5484D),
        place(0.0, 0.55, 0.0, 0.0, 0.0),
    );
    kick.bus = Some("Drums".to_string());
    let mut overheads = source(
        "oh",
        "Overheads",
        rgb(0xF07C5A),
        place(0.0, 0.7, 0.6, 0.9, 0.0),
    );
    overheads.bus = Some("Drums".to_string());
    let snapshot = RoomSnapshot {
        licensed,
        saved_format: format,
        format: if licensed {
            format
        } else {
            SpatialFormat::Stereo
        },
        sources: if licensed {
            vec![
                source(
                    "lead",
                    "Lead Vocal",
                    rgb(0x2FC9D6),
                    place(0.0, 1.0, 0.0, 0.0, 0.0),
                ),
                source(
                    "pad",
                    "Pad",
                    rgb(0x8B7CF6),
                    place(0.0, -0.6, 0.35, 1.0, 0.45),
                ),
                kick,
                overheads,
                source(
                    "bass",
                    "Bass",
                    rgb(0x46A758),
                    place(-0.2, 0.9, 0.0, 0.2, 0.0),
                ),
                fx,
            ]
        } else {
            Vec::new()
        },
        speaker_peaks: vec![
            0.42, 0.40, 0.71, 0.3, 0.12, 0.10, 0.2, 0.22, 0.05, 0.08, 0.02, 0.18,
        ],
        selected: Some("pad".to_string()),
    };
    let noop_id: Arc<dyn Fn(&str, &mut App)> = Arc::new(|_, _| {});
    let callbacks = RoomCallbacks {
        on_drag_start: noop_id.clone(),
        on_drag_preview: Arc::new(|_, _, _| {}),
        on_drag_commit: noop_id.clone(),
        on_select: noop_id,
        on_open_project_settings: Arc::new(|_| {}),
    };
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| RoomWindow::new(snapshot, callbacks, cx))
    })?;
    Ok(handle.into())
}
