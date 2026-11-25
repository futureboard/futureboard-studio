use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::registry::PluginFormat;

/// Scan target format for the isolated scanner process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginScanFormat {
    Vst3,
    Vst2,
    Clap,
    #[serde(rename = "audiounit")]
    AudioUnit,
}

impl PluginScanFormat {
    pub fn cli_arg(self) -> &'static str {
        match self {
            Self::Vst3 => "vst3",
            Self::Vst2 => "vst2",
            Self::Clap => "clap",
            Self::AudioUnit => "audiounit",
        }
    }

    pub fn from_cli(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "vst3" => Some(Self::Vst3),
            "vst2" => Some(Self::Vst2),
            "clap" => Some(Self::Clap),
            "audiounit" | "au" => Some(Self::AudioUnit),
            _ => None,
        }
    }

    pub fn registry_format(self) -> PluginFormat {
        match self {
            Self::Vst3 => PluginFormat::Vst3,
            Self::Vst2 => PluginFormat::Vst2,
            Self::Clap => PluginFormat::Clap,
            Self::AudioUnit => PluginFormat::Au,
        }
    }

    pub fn available_on_current_platform(self) -> bool {
        match self {
            Self::Vst3 | Self::Clap => true,
            // No VST2 module loader on Linux — the runtime bridge reports the
            // same, so listing plug-ins that could never be instantiated would
            // be a lie.
            Self::Vst2 => cfg!(any(target_os = "windows", target_os = "macos")),
            Self::AudioUnit => cfg!(target_os = "macos"),
        }
    }
}

/// Per-plugin scan lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScanStatus {
    #[default]
    Pending,
    Scanning,
    Success,
    Failed,
    Crashed,
    Skipped,
}

impl PluginScanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Scanning => "scanning",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Crashed => "crashed",
            Self::Skipped => "skipped",
        }
    }

    pub fn from_str_lossy(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "scanning" => Self::Scanning,
            "success" | "ok" => Self::Success,
            "failed" => Self::Failed,
            "crashed" => Self::Crashed,
            "skipped" => Self::Skipped,
            "metadata_only" => Self::Failed,
            "disabled" => Self::Skipped,
            _ => Self::Pending,
        }
    }
}

/// Structured scan failure reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail")]
pub enum PluginScanError {
    UnsupportedPlatform,
    AudioUnitUnavailable,
    AudioUnitEnumerationFailed(String),
    AudioUnitMetadataFailed(String),
    AudioUnitInstantiationFailed(String),
    AudioUnitScannerCrashed {
        exit_code: Option<i32>,
    },
    /// The out-of-process scanner child exited abnormally while scanning the
    /// given format (used for VST3/CLAP isolation, not just AudioUnit).
    ScannerProcessCrashed {
        format: PluginScanFormat,
        exit_code: Option<i32>,
    },
    InvalidComponent(String),
    NullComponentName,
    ScannerBinaryMissing(String),
    ScannerLaunchFailed(String),
    ScannerOutputInvalid(String),
    /// The scanner child exceeded its wall-clock budget and was killed. Some
    /// plug-ins block forever while their module loads; without this the whole
    /// scan stalls and no catalog is ever written.
    ScannerTimedOut {
        format: PluginScanFormat,
        seconds: u64,
    },
    PathMissing(String),
    NativeScanFailed(String),
}

impl PluginScanError {
    pub fn message(&self) -> String {
        match self {
            Self::UnsupportedPlatform => {
                "AudioUnit scanning is unavailable on this platform.".into()
            }
            Self::AudioUnitUnavailable => "AudioUnit scanning is unavailable.".into(),
            Self::AudioUnitEnumerationFailed(reason) => {
                format!("AudioUnit enumeration failed: {reason}")
            }
            Self::AudioUnitMetadataFailed(reason) => {
                format!("AudioUnit metadata read failed: {reason}")
            }
            Self::AudioUnitInstantiationFailed(reason) => {
                format!("AudioUnit instantiation failed: {reason}")
            }
            Self::AudioUnitScannerCrashed { exit_code } => match exit_code {
                Some(code) => format!("AudioUnit scan process crashed (exit {code})"),
                None => "AudioUnit scan process crashed".into(),
            },
            Self::ScannerProcessCrashed { format, exit_code } => match exit_code {
                Some(code) => format!("{} scan process crashed (exit {code})", format.cli_arg()),
                None => format!("{} scan process crashed", format.cli_arg()),
            },
            Self::InvalidComponent(detail) => format!("Invalid AudioUnit component: {detail}"),
            Self::NullComponentName => "AudioUnit component name was missing".into(),
            Self::ScannerBinaryMissing(path) => {
                format!("Plugin scanner binary not found: {path}")
            }
            Self::ScannerLaunchFailed(reason) => {
                format!("Failed to launch plugin scanner: {reason}")
            }
            Self::ScannerOutputInvalid(reason) => format!("Invalid scanner output: {reason}"),
            Self::ScannerTimedOut { format, seconds } => format!(
                "{} scan timed out after {seconds}s and was stopped",
                format.cli_arg()
            ),
            Self::PathMissing(path) => format!("Scan path does not exist: {path}"),
            Self::NativeScanFailed(reason) => reason.clone(),
        }
    }
}

/// Unified plug-in descriptor returned by scanners.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    pub id: String,
    pub format: String,
    pub name: String,
    pub vendor: String,
    pub version: Option<String>,
    pub path_or_identifier: String,
    pub category: String,
    pub is_instrument: bool,
    pub is_effect: bool,
    pub scan_status: PluginScanStatus,
    pub error_message: Option<String>,
    #[serde(default)]
    pub class_id: Option<String>,
    #[serde(default)]
    pub sub_categories: Option<String>,
    /// The plug-in exposes an ARA main factory. Only the VST3 scanner sets it.
    #[serde(default)]
    pub is_ara: bool,
    /// The VST3 component exposes an event output bus. Only the VST3 scanner
    /// sets it (by instantiating the component once to read its buses).
    #[serde(default)]
    pub midi_output: bool,
    #[serde(default)]
    pub sdk_metadata_loaded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanFailureRecord {
    pub path: String,
    pub error: String,
    #[serde(default)]
    pub scan_status: PluginScanStatus,
}

/// JSON payload emitted by the isolated scanner process.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResultPayload {
    pub format: PluginScanFormat,
    pub success: bool,
    pub plugins: Vec<PluginDescriptor>,
    #[serde(default)]
    pub failures: Vec<ScanFailureRecord>,
    #[serde(default)]
    pub crashed_plugins: Vec<ScanFailureRecord>,
    #[serde(default)]
    pub process_crashed: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub scanned_paths: Vec<PathBuf>,
}

impl ScanResultPayload {
    pub fn empty(format: PluginScanFormat) -> Self {
        Self {
            format,
            success: true,
            plugins: Vec::new(),
            failures: Vec::new(),
            crashed_plugins: Vec::new(),
            process_crashed: false,
            exit_code: None,
            error: None,
            scanned_paths: Vec::new(),
        }
    }

    pub fn process_crash(format: PluginScanFormat, exit_code: Option<i32>, error: String) -> Self {
        Self {
            format,
            success: false,
            plugins: Vec::new(),
            failures: Vec::new(),
            crashed_plugins: Vec::new(),
            process_crashed: true,
            exit_code,
            error: Some(error),
            scanned_paths: Vec::new(),
        }
    }
}
