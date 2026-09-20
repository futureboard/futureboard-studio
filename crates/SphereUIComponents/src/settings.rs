use crate::frame_scheduler::FrameRateMode;
use crate::paths::FutureboardPaths;
use gpui::AppContext;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub use sphere_midi_service::{MidiDeviceDirection, MidiDeviceSetting, MidiHardwareSettings};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectDefaults {
    pub tempo: f64,
    pub time_signature_num: u32,
    pub time_signature_den: u32,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub tracks_count: u32,
}

impl Default for ProjectDefaults {
    fn default() -> Self {
        Self {
            tempo: 120.0,
            time_signature_num: 4,
            time_signature_den: 4,
            sample_rate: 48000,
            buffer_size: 256,
            tracks_count: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutosaveSettings {
    pub enabled: bool,
    pub interval_minutes: u32,
    pub max_backups: u32,
}

impl Default for AutosaveSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: 5,
            max_backups: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotificationSettings {
    pub enable_warnings: bool,
    pub enable_system_notifications: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Nightly,
    Beta,
    #[default]
    Stable,
}

impl UpdateChannel {
    pub const ALL: [Self; 3] = [Self::Stable, Self::Beta, Self::Nightly];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Nightly => "Nightly",
            Self::Beta => "Beta",
            Self::Stable => "Stable",
        }
    }
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enable_warnings: true,
            enable_system_notifications: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GeneralSettings {
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_true")]
    pub show_start_screen: bool,
    #[serde(default = "default_true")]
    pub check_updates: bool,
    #[serde(default)]
    pub update_channel: UpdateChannel,
    #[serde(default = "default_true")]
    pub discord_rpc_enabled: bool,
    /// Active keyboard shortcut profile id (e.g. `"default"`, `"ableton-live"`).
    /// Matches [`crate::keymap::PROFILE_DESCRIPTORS`] ids. Persisted so a chosen
    /// keymap survives a restart; an unknown id falls back to the default map.
    #[serde(default = "default_keymap_profile")]
    pub keymap_profile: String,
    /// True after the user has answered the first-launch plug-in scan prompt.
    /// This records that the choice was made, not whether they chose to scan.
    #[serde(default)]
    pub plugin_scan_prompt_answered: bool,
    /// User-configured default directory for new projects. When `None` or empty
    /// the platform default ([`crate::project::io::default_projects_dir`]) is
    /// used. Resolve through [`GeneralSettings::resolved_default_project_dir`].
    #[serde(default)]
    pub default_project_directory: Option<PathBuf>,
    #[serde(default)]
    pub project_defaults: ProjectDefaults,
    #[serde(default)]
    pub autosave: AutosaveSettings,
    #[serde(default)]
    pub notifications: NotificationSettings,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            language: default_language(),
            show_start_screen: default_true(),
            check_updates: default_true(),
            update_channel: UpdateChannel::default(),
            discord_rpc_enabled: default_true(),
            keymap_profile: default_keymap_profile(),
            plugin_scan_prompt_answered: false,
            default_project_directory: None,
            project_defaults: ProjectDefaults::default(),
            autosave: AutosaveSettings::default(),
            notifications: NotificationSettings::default(),
        }
    }
}

impl GeneralSettings {
    /// Resolve the effective default project directory. Falls back to the
    /// platform default when unset, empty, or whitespace-only. Never panics.
    pub fn resolved_default_project_dir(&self) -> PathBuf {
        match &self.default_project_directory {
            Some(path) if !path.as_os_str().is_empty() => path.clone(),
            _ => crate::project::io::default_projects_dir(),
        }
    }

    /// True when the user has explicitly configured a default project directory
    /// (as opposed to relying on the platform fallback).
    pub fn has_configured_project_dir(&self) -> bool {
        self.default_project_directory
            .as_ref()
            .is_some_and(|p| !p.as_os_str().is_empty())
    }
}

fn default_language() -> String {
    "en".to_string()
}

/// Default keyboard shortcut profile id. Mirrors [`KeymapManager`]'s own
/// startup default so an absent `keymap_profile` in an older settings file
/// resolves to the same map the app has always shipped.
fn default_keymap_profile() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

fn default_audio_driver_type() -> String {
    #[cfg(target_os = "windows")]
    {
        "WASAPI Shared".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "CoreAudio".to_string()
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // Match the engine's platform default (`BackendKind::Auto` → cpal/ALSA,
        // including PipeWire's ALSA plugin). Explicit "ALSA" remains selectable.
        "Auto".to_string()
    }
}

/// Driver types this build/edition can actually open. Mirrors the Settings
/// Audio Driver dropdown so a Community Edition process never keeps an
/// Exclusive-only selection (notably `"ASIO"`) alive after settings load.
pub fn available_audio_driver_types() -> Vec<String> {
    // Prefer the engine's platform inventory so Settings cannot advertise a
    // backend DAUx cannot open. Labels stay the short UI names users already
    // know (not the longer `DAUx …` display names).
    let from_engine: Vec<String> = DirectAudio::backend::list_available_backends()
        .into_iter()
        .filter(|backend| backend.available)
        .filter_map(|backend| ui_label_for_backend_id(&backend.id))
        .collect();
    if !from_engine.is_empty() {
        return from_engine;
    }

    // Defensive fallback if the engine list is empty (should not happen).
    #[cfg(target_os = "windows")]
    {
        let mut backends = vec![
            "WASAPI Shared".to_string(),
            "WASAPI Exclusive".to_string(),
            "WDM-KS".to_string(),
        ];
        if DirectAudio::asio_support_enabled() {
            backends.push("ASIO".to_string());
        }
        backends
    }
    #[cfg(target_os = "macos")]
    {
        vec!["Auto".to_string(), "CoreAudio".to_string()]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        vec!["Auto".to_string(), "ALSA".to_string()]
    }
}

fn ui_label_for_backend_id(id: &str) -> Option<String> {
    match id {
        "auto" => Some("Auto".to_string()),
        "wasapi-shared" => Some("WASAPI Shared".to_string()),
        "wasapi-exclusive" => Some("WASAPI Exclusive".to_string()),
        "wdm-ks" => Some("WDM-KS".to_string()),
        "asio" => Some("ASIO".to_string()),
        "coreaudio" => Some("CoreAudio".to_string()),
        "alsa" => Some("ALSA".to_string()),
        // Legacy MME stays engine-only unless product UI asks for it.
        "mme" => None,
        _ => None,
    }
}

/// Map a persisted `driver_type` onto a backend this build can drive.
///
/// Professional Edition may write `"ASIO"` into the shared `settings.json`. When
/// Community Edition (or an Exclusive build without ASIO entitlement) loads
/// that file, the selection must fall back to the platform default — typically
/// WASAPI Shared on Windows — instead of leaving a latent ASIO pin that the UI
/// only half-sanitizes for display.
pub fn sanitize_audio_driver_type(driver_type: &str) -> String {
    let available = available_audio_driver_types();
    if available.iter().any(|backend| backend == driver_type) {
        driver_type.to_string()
    } else {
        let platform_default = default_audio_driver_type();
        available
            .into_iter()
            .find(|backend| backend == &platform_default)
            .unwrap_or(platform_default)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioHardwareSettings {
    pub driver_type: String,
    pub device_in: String,
    pub device_out: String,
    pub active_inputs: Vec<u32>,
    pub active_outputs: Vec<u32>,
}

impl Default for AudioHardwareSettings {
    fn default() -> Self {
        Self {
            driver_type: default_audio_driver_type(),
            device_in: "System Default Input".to_string(),
            device_out: "System Default Output".to_string(),
            active_inputs: vec![0, 1],
            active_outputs: vec![0, 1],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncSettings {
    pub clock_source: String,
    pub ltc_enabled: bool,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            clock_source: "Internal".to_string(),
            ltc_enabled: false,
        }
    }
}

/// One logical bus in the application-level **Default Audio Connections**
/// template (Audio Setup → Default Audio Connections).
///
/// The template is a *recipe*, not a live registry: it is copied into a new
/// project, which then owns fresh project-local ids. Editing the template never
/// reaches back into an existing project, and a project never writes back into
/// it. Deliberately carries no window geometry and no runtime status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DefaultAudioConnection {
    pub name: String,
    /// `"input"` / `"output"`.
    pub direction: String,
    /// `"mono"` / `"stereo"`.
    pub channel_layout: String,
    /// First device port index bound to logical channel 0. Ports are taken
    /// consecutively from here, against whatever device the new project's
    /// hardware actually exposes.
    pub first_port_index: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// The application-level Default Audio Connections template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DefaultAudioConnectionsSettings {
    #[serde(default)]
    pub inputs: Vec<DefaultAudioConnection>,
    #[serde(default)]
    pub outputs: Vec<DefaultAudioConnection>,
}

impl Default for DefaultAudioConnectionsSettings {
    /// Mirrors the built-in template a first-run project gets: two mono inputs,
    /// a stereo input pair, and a stereo main output. Rows whose ports the
    /// hardware does not expose are skipped at copy time rather than created as
    /// phantoms.
    fn default() -> Self {
        Self {
            inputs: vec![
                DefaultAudioConnection {
                    name: "Mono Input 1".to_string(),
                    direction: "input".to_string(),
                    channel_layout: "mono".to_string(),
                    first_port_index: 0,
                    enabled: true,
                },
                DefaultAudioConnection {
                    name: "Mono Input 2".to_string(),
                    direction: "input".to_string(),
                    channel_layout: "mono".to_string(),
                    first_port_index: 1,
                    enabled: true,
                },
                DefaultAudioConnection {
                    name: "Stereo Input 1-2".to_string(),
                    direction: "input".to_string(),
                    channel_layout: "stereo".to_string(),
                    first_port_index: 0,
                    enabled: true,
                },
            ],
            outputs: vec![DefaultAudioConnection {
                name: "Main Output".to_string(),
                direction: "output".to_string(),
                channel_layout: "stereo".to_string(),
                first_port_index: 0,
                enabled: true,
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HardwareSettings {
    #[serde(default)]
    pub audio: AudioHardwareSettings,
    /// Copied into each new project; never linked to one afterwards.
    #[serde(default)]
    pub default_audio_connections: DefaultAudioConnectionsSettings,
    #[serde(default)]
    pub midi: MidiHardwareSettings,
    #[serde(default)]
    pub control_surfaces: Vec<String>,
    #[serde(default)]
    pub sync: SyncSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArrangementAppearanceSettings {
    pub grid_line_intensity: f32,
    pub clip_color_mode: String,
}

impl Default for ArrangementAppearanceSettings {
    fn default() -> Self {
        Self {
            grid_line_intensity: 0.4,
            clip_color_mode: "TrackAccent".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PianoRollAppearanceSettings {
    pub show_key_guides: bool,
}

impl Default for PianoRollAppearanceSettings {
    fn default() -> Self {
        Self {
            show_key_guides: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MixerAppearanceSettings {
    pub meter_decay_db_per_sec: f32,
    pub peak_hold_seconds: f32,
}

impl Default for MixerAppearanceSettings {
    fn default() -> Self {
        Self {
            meter_decay_db_per_sec: 24.0,
            peak_hold_seconds: 3.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppearanceSettings {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    /// Which OS text stack draws glyphs. Read once at startup — see
    /// [`TextRenderingBackend`].
    #[serde(default)]
    pub text_rendering: TextRenderingBackend,
    #[serde(default)]
    pub arrangement: ArrangementAppearanceSettings,
    #[serde(default)]
    pub piano_roll: PianoRollAppearanceSettings,
    #[serde(default)]
    pub mixer: MixerAppearanceSettings,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            ui_scale: default_ui_scale(),
            text_rendering: TextRenderingBackend::default(),
            arrangement: ArrangementAppearanceSettings::default(),
            piano_roll: PianoRollAppearanceSettings::default(),
            mixer: MixerAppearanceSettings::default(),
        }
    }
}

/// Text rasterization backend.
///
/// DirectWrite is the default and the only one with subpixel antialiasing,
/// colour emoji, and full OpenType shaping. GDI+ is a fallback for machines
/// where DirectWrite fails — a corrupted Windows font cache, a font manager
/// that hooks `DWrite.dll`, or a locked-down image — which otherwise leaves the
/// app running with no visible text at all.
///
/// The text system is built before the settings file is loaded, so this is read
/// straight from disk at startup and a change only takes effect on the next
/// launch. It is deliberately not live-switchable: swapping the text system
/// would invalidate every cached glyph, line layout, and atlas page mid-frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TextRenderingBackend {
    #[default]
    DirectWrite,
    Gdi,
}

impl TextRenderingBackend {
    pub const ALL: [Self; 2] = [Self::DirectWrite, Self::Gdi];

    /// Display name. The default is marked as such because choosing the other
    /// one is a troubleshooting step, not a preference.
    pub fn label(self) -> &'static str {
        match self {
            Self::DirectWrite => "DirectWrite",
            Self::Gdi => "GDI+",
        }
    }

    /// Stable token shared with the platform layer and the `GPUI_TEXT_BACKEND`
    /// escape hatch.
    pub fn as_token(self) -> &'static str {
        match self {
            Self::DirectWrite => "directwrite",
            Self::Gdi => "gdi",
        }
    }
}

fn default_theme() -> String {
    "futureboard.default".to_string()
}

fn default_ui_scale() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MouseEditingSettings {
    pub zoom_sensitivity: f32,
    pub natural_scroll: bool,
}

impl Default for MouseEditingSettings {
    fn default() -> Self {
        Self {
            zoom_sensitivity: 1.0,
            natural_scroll: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapEditingSettings {
    pub snap_to_grid: bool,
    pub default_snap_value: String,
}

impl Default for SnapEditingSettings {
    fn default() -> Self {
        Self {
            snap_to_grid: true,
            default_snap_value: "1/16".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEditingSettings {
    pub max_undo_steps: u32,
}

impl Default for HistoryEditingSettings {
    fn default() -> Self {
        Self {
            max_undo_steps: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct EditingSettings {
    #[serde(default)]
    pub mouse: MouseEditingSettings,
    #[serde(default)]
    pub snap: SnapEditingSettings,
    #[serde(default)]
    pub history: HistoryEditingSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioRecordingSettings {
    pub format: String,
    pub bit_depth: u32,
    pub recording_path: String,
    #[serde(default)]
    pub recording_offset_ms: i32,
    #[serde(default = "default_true")]
    pub save_before_recording: bool,
    #[serde(default = "default_true")]
    pub generate_waveform_after_record: bool,
}

impl Default for AudioRecordingSettings {
    fn default() -> Self {
        Self {
            format: "wav".to_string(),
            bit_depth: 24,
            recording_path: String::new(),
            recording_offset_ms: 0,
            save_before_recording: true,
            generate_waveform_after_record: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DefaultMonitorMode {
    Off,
    Auto,
    Input,
}

impl Default for DefaultMonitorMode {
    fn default() -> Self {
        Self::Off
    }
}

impl DefaultMonitorMode {
    pub fn add_track_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::Input => "input",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MetronomeSettings {
    pub enabled: bool,
    pub volume: f32,
    pub sound_type: String,
    /// Whether record count-in is active. The bar count is kept separately so
    /// toggling count-in off does not discard the user's chosen duration.
    pub count_in_enabled: bool,
    pub count_in_bars: u32,
}

impl Default for MetronomeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            volume: 0.8,
            sound_type: "Woodblock".to_string(),
            count_in_enabled: true,
            count_in_bars: 1,
        }
    }
}

impl<'de> Deserialize<'de> for MetronomeSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredMetronomeSettings {
            #[serde(default)]
            enabled: bool,
            #[serde(default = "default_metronome_volume")]
            volume: f32,
            #[serde(default = "default_metronome_sound")]
            sound_type: String,
            count_in_enabled: Option<bool>,
            #[serde(default = "default_count_in_bars")]
            count_in_bars: u32,
        }

        let stored = StoredMetronomeSettings::deserialize(deserializer)?;
        // Before `count_in_enabled` existed, zero bars was the persisted off
        // state. Preserve that state while migrating the duration to one bar.
        let count_in_enabled = stored.count_in_enabled.unwrap_or(stored.count_in_bars > 0);
        Ok(Self {
            enabled: stored.enabled,
            volume: stored.volume,
            sound_type: stored.sound_type,
            count_in_enabled,
            count_in_bars: stored.count_in_bars.max(1),
        })
    }
}

fn default_metronome_volume() -> f32 {
    0.8
}

fn default_metronome_sound() -> String {
    "Woodblock".to_string()
}

fn default_count_in_bars() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RecordingSettings {
    #[serde(default)]
    pub audio: AudioRecordingSettings,
    #[serde(default)]
    pub default_monitor_mode: DefaultMonitorMode,
    #[serde(default)]
    pub metronome: MetronomeSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Vst3PluginSettings {
    pub enabled: bool,
    pub paths: Vec<String>,
}

impl Default for Vst3PluginSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: default_vst3_paths(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClapPluginSettings {
    pub enabled: bool,
    pub paths: Vec<String>,
}

impl Default for ClapPluginSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: default_clap_paths(),
        }
    }
}

fn default_vst3_paths() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        vec![
            r"C:\Program Files\Common Files\VST3".to_string(),
            r"C:\Program Files (x86)\Common Files\VST3".to_string(),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        let mut paths = vec!["/Library/Audio/Plug-Ins/VST3".to_string()];
        if let Some(home) = dirs::home_dir() {
            paths.push(
                home.join("Library/Audio/Plug-Ins/VST3")
                    .display()
                    .to_string(),
            );
        }
        paths
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut paths = vec![
            "/usr/lib/vst3".to_string(),
            "/usr/local/lib/vst3".to_string(),
        ];
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".vst3").display().to_string());
        }
        paths
    }
}

fn default_clap_paths() -> Vec<String> {
    #[cfg(target_os = "windows")]
    {
        vec![r"C:\Program Files\Common Files\CLAP".to_string()]
    }
    #[cfg(target_os = "macos")]
    {
        let mut paths = vec!["/Library/Audio/Plug-Ins/CLAP".to_string()];
        if let Some(home) = dirs::home_dir() {
            paths.push(
                home.join("Library/Audio/Plug-Ins/CLAP")
                    .display()
                    .to_string(),
            );
        }
        paths
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut paths = vec![
            "/usr/lib/clap".to_string(),
            "/usr/local/lib/clap".to_string(),
        ];
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".clap").display().to_string());
        }
        paths
    }
}

fn migrate_foreign_plugin_defaults(paths: &mut Vec<String>, local_defaults: Vec<String>) {
    const KNOWN_DEFAULTS: &[&str] = &[
        r"C:\Program Files\Common Files\VST3",
        r"C:\Program Files (x86)\Common Files\VST3",
        r"C:\Program Files\Common Files\CLAP",
        "/Library/Audio/Plug-Ins/VST3",
        "/Library/Audio/Plug-Ins/CLAP",
        "/usr/lib/vst3",
        "/usr/local/lib/vst3",
        "/usr/lib/clap",
        "/usr/local/lib/clap",
    ];
    paths.retain(|path| {
        !KNOWN_DEFAULTS.contains(&path.as_str()) || local_defaults.iter().any(|local| local == path)
    });
    if paths.is_empty() {
        *paths = local_defaults;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanPluginSettings {
    pub background_scan: bool,
    pub failed_plugins: Vec<String>,
}

impl Default for ScanPluginSettings {
    fn default() -> Self {
        Self {
            background_scan: true,
            failed_plugins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PluginsSettings {
    #[serde(default)]
    pub vst3: Vst3PluginSettings,
    #[serde(default)]
    pub clap: ClapPluginSettings,
    #[serde(default)]
    pub scan: ScanPluginSettings,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RenderMode {
    /// Let Futureboard choose the safe accelerated default per surface.
    Auto,
    /// Opt into the experimental GPU primitive layer (batched `canvas` paint).
    GpuAcceleration,
    /// Force the legacy CPU/`div` rendering path everywhere.
    CpuRender,
}

impl Default for RenderMode {
    fn default() -> Self {
        RenderMode::Auto
    }
}

impl RenderMode {
    pub fn label(self) -> &'static str {
        match self {
            RenderMode::Auto => "Auto",
            RenderMode::GpuAcceleration => "GPU (Experimental)",
            RenderMode::CpuRender => "CPU Render",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value")]
pub enum GpuDevicePreference {
    Auto,
    DeviceId(String),
}

impl Default for GpuDevicePreference {
    fn default() -> Self {
        GpuDevicePreference::Auto
    }
}

impl GpuDevicePreference {
    /// Stable string id for matching against detected GPU adapter ids.
    pub fn id(&self) -> &str {
        match self {
            GpuDevicePreference::Auto => "auto",
            GpuDevicePreference::DeviceId(id) => id.as_str(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PerformanceSettings {
    #[serde(default)]
    pub render_mode: RenderMode,
    #[serde(default)]
    pub gpu_device: GpuDevicePreference,
    /// Frame pacing mode. `DisplaySync` (default) tracks the monitor refresh
    /// rate; the fixed/battery modes are caps for debug/low-power. Overridable
    /// at runtime via `FUTUREBOARD_FRAME_RATE_MODE`.
    #[serde(default)]
    pub frame_rate: FrameRateMode,
    /// Compact FPS / frame-time pill in the status bar (View → Developer).
    #[serde(default)]
    pub show_status_performance_metrics: bool,
    /// Floating verbose performance overlay (View → Developer).
    #[serde(default)]
    pub show_performance_overlay: bool,
}

/// Dropout Protection mode (Settings → Playback). Keeps internal headroom
/// against control/UI/plugin jitter, independent of the device buffer size.
/// Maps 1:1 to `DirectAudio::DropoutProtectionMode`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DropoutProtectionMode {
    /// Lowest latency, minimal safety margin.
    Off,
    /// Small safety margin / conservative scheduling.
    Light,
    /// Default recommended mode — better protection during UI activity.
    Medium,
    /// Maximum stability (may add internal latency).
    High,
}

impl Default for DropoutProtectionMode {
    fn default() -> Self {
        DropoutProtectionMode::Medium
    }
}

impl DropoutProtectionMode {
    pub fn label(self) -> &'static str {
        match self {
            DropoutProtectionMode::Off => "Off",
            DropoutProtectionMode::Light => "Light",
            DropoutProtectionMode::Medium => "Medium (Recommended)",
            DropoutProtectionMode::High => "High",
        }
    }

    pub const ALL: [DropoutProtectionMode; 4] = [
        DropoutProtectionMode::Off,
        DropoutProtectionMode::Light,
        DropoutProtectionMode::Medium,
        DropoutProtectionMode::High,
    ];
}

/// What Space does while the arrangement is focused and not typing into an input.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SpacebarAction {
    /// Start playback, or pause in place.
    #[default]
    PlayPause,
    /// Start playback, or stop and return the playhead to where Play began.
    PlayStop,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlaybackSettings {
    /// Align parallel track paths at the master bus (Phase W PDC).
    #[serde(default = "default_true")]
    pub latency_compensation: bool,
    /// Realtime dropout protection mode.
    #[serde(default)]
    pub dropout_protection: DropoutProtectionMode,
    /// Spacebar play/pause vs play/stop.
    #[serde(default)]
    pub spacebar_action: SpacebarAction,
    /// When true, the Stop button returns the playhead to where Play began.
    #[serde(default)]
    pub return_playhead_on_stop: bool,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            latency_compensation: default_true(),
            dropout_protection: DropoutProtectionMode::default(),
            spacebar_action: SpacebarAction::default(),
            return_playhead_on_stop: false,
        }
    }
}

/// Live latency readout shown in Settings → Audio (polled from DirectAudio).
#[derive(Debug, Clone, Default)]
pub struct SettingsAudioLatencySnapshot {
    pub engine_open: bool,
    pub device_state: String,
    pub backend_name: String,
    pub last_error: Option<String>,
    /// Active runtime sample rate (Hz) — the rate the opened stream runs at.
    /// All timing uses this; the status bar shows this.
    pub active_sample_rate: u32,
    /// Sample rate requested at device open (Hz), or 0 for "device default".
    /// When this differs from `active_sample_rate` the UI warns that timing
    /// follows the active device rate.
    pub requested_sample_rate: u32,
    /// `true` when the user changed the preferred sample rate but chose "Later"
    /// (deferred the engine restart). Timing keeps using `active_sample_rate`
    /// until the project is re-opened / the engine restarted.
    pub restart_pending: bool,
    /// The deferred preferred sample rate (Hz) when `restart_pending`; 0 otherwise.
    pub deferred_sample_rate: u32,
    pub buffer_ms: f64,
    pub buffer_frames: u32,
    /// Input (capture) latency the backend reports, ms; `0` when unknown.
    pub input_ms: f64,
    /// Output (play-out) latency the backend reports/measures, ms.
    pub output_ms: f64,
    /// Reported round trip when the backend knows both halves (ASIO), else
    /// the classic two-buffer estimate.
    pub round_trip_ms: f64,
    /// `true` when `round_trip_ms` is the backend's own figure rather than
    /// the estimate, so the UI can say which it is showing.
    pub round_trip_reported: bool,
    pub max_path_ms: f64,
    pub max_path_samples: u32,
    pub master_plugin_ms: f64,
    pub pdc_active: bool,
    pub track_lines: Vec<SettingsTrackLatencyLine>,
}

#[derive(Debug, Clone, Default)]
pub struct SettingsTrackLatencyLine {
    pub track_id: String,
    pub plugin_ms: f64,
    pub path_ms: f64,
    pub pdc_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SettingsSchema {
    #[serde(default)]
    pub general: GeneralSettings,
    #[serde(default)]
    pub hardware: HardwareSettings,
    #[serde(default)]
    pub appearance: AppearanceSettings,
    #[serde(default)]
    pub editing: EditingSettings,
    #[serde(default)]
    pub recording: RecordingSettings,
    #[serde(default)]
    pub playback: PlaybackSettings,
    #[serde(default)]
    pub plugins: PluginsSettings,
    #[serde(default)]
    pub performance: PerformanceSettings,
}

impl SettingsSchema {
    /// Load and validate the settings schema directly from the resolved
    /// settings file. Useful for surfaces (e.g. the Welcome window) that run
    /// before the [`GlobalSettingsModel`] entity exists. Falls back to defaults
    /// on any read/parse error — never panics.
    pub fn load_from_disk() -> Self {
        let path = FutureboardPaths::resolve().settings_file;
        let mut schema = SettingsModel::load_from_path(&path);
        persist_sanitized_audio_driver(&path, &mut schema);
        schema
    }

    /// Persist only the default project directory back to the settings file,
    /// preserving every other field already on disk. Intended for the Welcome
    /// window, which has no [`SettingsModel`] entity. Best-effort; logs on
    /// failure and never panics.
    pub fn persist_default_project_directory(dir: Option<PathBuf>) {
        let path = FutureboardPaths::resolve().settings_file;
        let mut schema = SettingsModel::load_from_path(&path);
        schema.general.default_project_directory = dir.filter(|p| !p.as_os_str().is_empty());
        schema.validate_and_clamp();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&schema) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    eprintln!("[settings] failed to persist default project dir: {e}");
                }
            }
            Err(e) => eprintln!("[settings] failed to serialize settings: {e}"),
        }
    }

    /// Persist the first-launch plug-in scan prompt response from a pre-Studio
    /// surface, where the global [`SettingsModel`] may not exist yet.
    pub fn persist_plugin_scan_prompt_answered() {
        let path = FutureboardPaths::resolve().settings_file;
        let mut schema = SettingsModel::load_from_path(&path);
        schema.general.plugin_scan_prompt_answered = true;
        schema.validate_and_clamp();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&schema) {
            Ok(json) => {
                if let Err(error) = std::fs::write(&path, json) {
                    eprintln!("[settings] failed to persist plug-in scan prompt: {error}");
                }
            }
            Err(error) => eprintln!("[settings] failed to serialize settings: {error}"),
        }
    }

    pub fn validate_and_clamp(&mut self) {
        // Clamp tempo
        if self.general.project_defaults.tempo < 20.0 {
            self.general.project_defaults.tempo = 20.0;
        } else if self.general.project_defaults.tempo > 999.0 {
            self.general.project_defaults.tempo = 999.0;
        }

        // Clamp sample rate
        let sr = self.general.project_defaults.sample_rate;
        if sr != 44100 && sr != 48000 && sr != 88200 && sr != 96000 && sr != 192000 {
            self.general.project_defaults.sample_rate = 48000;
        }

        // Clamp buffer size (power of two between 32 and 4096)
        let buf = self.general.project_defaults.buffer_size;
        let is_valid_buf = buf >= 32 && buf <= 4096 && (buf & (buf - 1)) == 0;
        if !is_valid_buf {
            self.general.project_defaults.buffer_size = 256;
        }

        // Clamp ui scale
        if self.appearance.ui_scale < 0.5 {
            self.appearance.ui_scale = 0.5;
        } else if self.appearance.ui_scale > 2.5 {
            self.appearance.ui_scale = 2.5;
        }

        self.recording.metronome.count_in_bars = self.recording.metronome.count_in_bars.clamp(1, 4);

        // Older builds serialized Windows-only plugin defaults on every OS.
        // Remove only exact built-in defaults from foreign platforms; custom
        // folders remain untouched.
        migrate_foreign_plugin_defaults(&mut self.plugins.vst3.paths, default_vst3_paths());
        migrate_foreign_plugin_defaults(&mut self.plugins.clap.paths, default_clap_paths());

        // Exclusive → Community (or entitlement lost): remap unavailable audio
        // backends before the engine/UI read the schema. Clearing device picks
        // mirrors Settings' own driver-change behavior so an ASIO driver name
        // cannot stick under WASAPI Shared.
        let sanitized_driver = sanitize_audio_driver_type(&self.hardware.audio.driver_type);
        if sanitized_driver != self.hardware.audio.driver_type {
            eprintln!(
                "[settings] audio driver {:?} unavailable on this edition; falling back to {sanitized_driver}",
                self.hardware.audio.driver_type
            );
            self.hardware.audio.driver_type = sanitized_driver;
            self.hardware.audio.device_in.clear();
            self.hardware.audio.device_out.clear();
            self.hardware.audio.active_inputs = vec![0, 1];
            self.hardware.audio.active_outputs = vec![0, 1];
        }

        sphere_midi_service::migrate_legacy_midi_settings(&mut self.hardware.midi);
    }
}

impl SettingsAudioLatencySnapshot {
    pub fn from_engine(engine: &DirectAudio::AudioEngine) -> Self {
        let stats = engine.stats();
        let info = engine.latency_info();
        let mut track_lines: Vec<SettingsTrackLatencyLine> = info
            .tracks
            .iter()
            .filter(|track| track.plugin_samples > 0 || track.path_samples > 0)
            .map(|track| SettingsTrackLatencyLine {
                track_id: track.track_id.clone(),
                plugin_ms: track.plugin_ms,
                path_ms: track.path_ms,
                pdc_ms: track.pdc_delay_ms,
            })
            .collect();
        track_lines.sort_by(|a, b| {
            b.path_ms
                .partial_cmp(&a.path_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        track_lines.truncate(6);

        Self {
            engine_open: stats.stream_open,
            device_state: stats.device_state,
            backend_name: stats.backend_name,
            last_error: stats.last_error,
            active_sample_rate: stats.sample_rate,
            requested_sample_rate: stats.requested_sample_rate,
            // Overridden by the latency provider, which knows the deferred target.
            restart_pending: false,
            deferred_sample_rate: 0,
            buffer_ms: info.buffer_ms,
            buffer_frames: info.buffer_frames,
            input_ms: stats.input_latency_ms,
            output_ms: stats.estimated_latency_ms,
            round_trip_ms: if stats.round_trip_latency_ms > 0.0 {
                stats.round_trip_latency_ms
            } else {
                info.buffer_ms * 2.0
            },
            round_trip_reported: stats.round_trip_latency_ms > 0.0,
            max_path_ms: info.max_path_ms,
            max_path_samples: info.max_path_samples,
            master_plugin_ms: info.master_ms,
            pdc_active: info.pdc_enabled,
            track_lines,
        }
    }

    pub fn unavailable() -> Self {
        Self::default()
    }
}

pub struct SettingsModel {
    pub current: SettingsSchema,
    pub path: PathBuf,
}

pub type SettingsChangeListener = Arc<dyn Fn(&SettingsSchema) + Send + Sync + 'static>;

static SETTINGS_CHANGE_LISTENER: OnceLock<SettingsChangeListener> = OnceLock::new();

/// Installs the native shell's lightweight settings propagation hook.
pub fn install_settings_change_listener(listener: SettingsChangeListener) {
    let _ = SETTINGS_CHANGE_LISTENER.set(listener);
}

/// Validate settings and, when Exclusive-only audio drivers were remapped
/// (e.g. ASIO → WASAPI Shared on Community), write the healed schema back so
/// the latent pin does not survive across launches.
fn persist_sanitized_audio_driver(path: &Path, schema: &mut SettingsSchema) {
    let before = schema.hardware.audio.driver_type.clone();
    schema.validate_and_clamp();
    if schema.hardware.audio.driver_type == before {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&schema) {
        Ok(json) => {
            if let Err(error) = std::fs::write(path, json) {
                eprintln!("[settings] failed to persist sanitized audio driver: {error}");
            }
        }
        Err(error) => {
            eprintln!("[settings] failed to serialize sanitized audio driver: {error}");
        }
    }
}

impl SettingsModel {
    pub fn load_or_create(cx: &mut gpui::App) -> gpui::Entity<Self> {
        let path = FutureboardPaths::resolve().settings_file;
        let mut settings = Self::load_from_path(&path);
        persist_sanitized_audio_driver(&path, &mut settings);

        cx.new(|_cx| Self {
            current: settings,
            path,
        })
    }

    pub fn load_from_path(path: &Path) -> SettingsSchema {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match serde_json::from_str::<SettingsSchema>(&content) {
                    Ok(schema) => schema,
                    Err(e) => {
                        eprintln!("[settings] failed to parse settings.json: {e}. backing up.");
                        let backup_path = path.with_extension("json.backup");
                        let _ = std::fs::rename(path, &backup_path);
                        let default_schema = SettingsSchema::default();
                        if let Ok(json) = serde_json::to_string_pretty(&default_schema) {
                            let _ = std::fs::write(path, json);
                        }
                        default_schema
                    }
                },
                Err(e) => {
                    eprintln!("[settings] failed to read settings.json: {e}");
                    SettingsSchema::default()
                }
            }
        } else {
            let default_schema = SettingsSchema::default();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(&default_schema) {
                let _ = std::fs::write(path, json);
            }
            default_schema
        }
    }

    pub fn save_to_disk(&self) {
        let path = self.path.clone();
        let schema = self.current.clone();

        std::thread::spawn(move || {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(&schema) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        eprintln!("[settings] failed to write settings file: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("[settings] failed to serialize settings: {e}");
                }
            }
        });
    }

    pub fn update_setting<F>(&mut self, updater: F, cx: &mut gpui::Context<Self>)
    where
        F: FnOnce(&mut SettingsSchema),
    {
        updater(&mut self.current);
        self.current.validate_and_clamp();
        if let Some(listener) = SETTINGS_CHANGE_LISTENER.get() {
            listener(&self.current);
        }
        self.save_to_disk();
        cx.notify();
    }
}

pub struct GlobalSettingsModel(pub gpui::Entity<SettingsModel>);

impl gpui::Global for GlobalSettingsModel {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings() {
        let schema = SettingsSchema::default();
        assert_eq!(schema.general.language, "en");
        assert!(schema.general.discord_rpc_enabled);
        assert!(!schema.general.plugin_scan_prompt_answered);
        assert_eq!(schema.general.project_defaults.tempo, 120.0);
        assert_eq!(schema.general.project_defaults.sample_rate, 48000);
        assert_eq!(schema.general.project_defaults.buffer_size, 256);
        assert_eq!(schema.appearance.ui_scale, 1.0);
    }

    #[test]
    fn test_validation_clamping() {
        let mut schema = SettingsSchema::default();

        // Invalid tempo
        schema.general.project_defaults.tempo = 10.0;
        schema.validate_and_clamp();
        assert_eq!(schema.general.project_defaults.tempo, 20.0);

        schema.general.project_defaults.tempo = 1200.0;
        schema.validate_and_clamp();
        assert_eq!(schema.general.project_defaults.tempo, 999.0);

        // Invalid sample rate
        schema.general.project_defaults.sample_rate = 99999;
        schema.validate_and_clamp();
        assert_eq!(schema.general.project_defaults.sample_rate, 48000);

        // Invalid buffer size
        schema.general.project_defaults.buffer_size = 123;
        schema.validate_and_clamp();
        assert_eq!(schema.general.project_defaults.buffer_size, 256);

        // Invalid UI scale
        schema.appearance.ui_scale = 0.1;
        schema.validate_and_clamp();
        assert_eq!(schema.appearance.ui_scale, 0.5);

        schema.appearance.ui_scale = 5.0;
        schema.validate_and_clamp();
        assert_eq!(schema.appearance.ui_scale, 2.5);

        schema.recording.metronome.count_in_bars = 0;
        schema.validate_and_clamp();
        assert_eq!(schema.recording.metronome.count_in_bars, 1);
    }

    #[test]
    fn legacy_count_in_state_migrates_without_losing_off_state() {
        let off: MetronomeSettings = serde_json::from_str(
            r#"{"enabled":false,"volume":0.8,"sound_type":"Woodblock","count_in_bars":0}"#,
        )
        .unwrap();
        assert!(!off.count_in_enabled);
        assert_eq!(off.count_in_bars, 1);

        let on: MetronomeSettings = serde_json::from_str(
            r#"{"enabled":false,"volume":0.8,"sound_type":"Woodblock","count_in_bars":3}"#,
        )
        .unwrap();
        assert!(on.count_in_enabled);
        assert_eq!(on.count_in_bars, 3);
    }

    #[test]
    fn plugin_defaults_match_the_current_platform() {
        let settings = PluginsSettings::default();
        #[cfg(target_os = "windows")]
        {
            assert!(settings.vst3.paths.iter().all(|path| path.contains(':')));
            assert!(settings.clap.paths.iter().all(|path| path.contains(':')));
        }
        #[cfg(not(target_os = "windows"))]
        {
            assert!(settings.vst3.paths.iter().all(|path| !path.contains(r":\")));
            assert!(settings.clap.paths.iter().all(|path| !path.contains(r":\")));
        }
    }

    #[test]
    fn unavailable_asio_driver_falls_back_to_platform_default() {
        // Community / unentitled builds must not keep an Exclusive ASIO pin.
        if DirectAudio::asio_support_enabled() {
            return;
        }
        let mut schema = SettingsSchema::default();
        schema.hardware.audio.driver_type = "ASIO".to_string();
        schema.hardware.audio.device_out = "Focusrite USB ASIO".to_string();
        schema.hardware.audio.device_in = "Focusrite USB ASIO".to_string();
        schema.validate_and_clamp();
        assert_eq!(
            schema.hardware.audio.driver_type,
            default_audio_driver_type()
        );
        assert!(schema.hardware.audio.device_out.is_empty());
        assert!(schema.hardware.audio.device_in.is_empty());
    }

    #[test]
    fn test_corrupt_file_recovery() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_settings_corrupt.json");
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }

        // Write invalid JSON
        std::fs::write(&path, "{ invalid json ... }").unwrap();

        // Load it
        let schema = SettingsModel::load_from_path(&path);

        // Should have backed up and loaded defaults
        assert_eq!(schema.general.project_defaults.tempo, 120.0);

        let backup_path = path.with_extension("json.backup");
        assert!(backup_path.exists());

        // Clean up
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup_path);
    }
}

/// The text backend is read off disk before the app exists, so its defaulting
/// and its serialized spelling are the contract that keeps an old settings file
/// — or a hand-edited one — from changing how the app draws.
#[cfg(test)]
mod text_rendering_settings_tests {
    use super::*;

    #[test]
    fn directwrite_is_the_default() {
        assert_eq!(
            AppearanceSettings::default().text_rendering,
            TextRenderingBackend::DirectWrite
        );
        assert_eq!(
            SettingsSchema::default().appearance.text_rendering,
            TextRenderingBackend::DirectWrite
        );
    }

    /// Every settings file written before this option existed has no such key.
    #[test]
    fn a_settings_file_without_the_key_still_loads() {
        let schema: SettingsSchema =
            serde_json::from_str(r#"{"appearance":{"theme":"futureboard.default"}}"#)
                .expect("a partial settings file must still parse");
        assert_eq!(
            schema.appearance.text_rendering,
            TextRenderingBackend::DirectWrite
        );
    }

    #[test]
    fn it_round_trips_through_the_settings_file() {
        for backend in TextRenderingBackend::ALL {
            let mut schema = SettingsSchema::default();
            schema.appearance.text_rendering = backend;
            let json = serde_json::to_string(&schema).expect("serialize");
            let parsed: SettingsSchema = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed.appearance.text_rendering, backend);
        }
    }

    /// The on-disk spelling is part of the contract with the platform layer and
    /// with anyone editing the file by hand to recover a blank-text install.
    #[test]
    fn the_serialized_names_are_stable() {
        assert_eq!(
            serde_json::to_string(&TextRenderingBackend::DirectWrite).unwrap(),
            "\"direct-write\""
        );
        assert_eq!(
            serde_json::to_string(&TextRenderingBackend::Gdi).unwrap(),
            "\"gdi\""
        );
    }

    #[test]
    fn every_backend_has_a_label_and_a_token() {
        for backend in TextRenderingBackend::ALL {
            assert!(!backend.label().is_empty());
            assert!(!backend.as_token().is_empty());
        }
        assert_eq!(TextRenderingBackend::Gdi.label(), "GDI+");
        assert_eq!(TextRenderingBackend::DirectWrite.as_token(), "directwrite");
    }

    #[test]
    fn playback_transport_fields_default_when_missing() {
        let parsed: PlaybackSettings =
            serde_json::from_str(r#"{"latency_compensation":true}"#).expect("deserialize");
        assert_eq!(parsed.spacebar_action, SpacebarAction::PlayPause);
        assert!(!parsed.return_playhead_on_stop);
        assert_eq!(
            serde_json::to_string(&SpacebarAction::PlayStop).unwrap(),
            "\"play-stop\""
        );
    }
}
