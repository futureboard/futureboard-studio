//! DAUx — cross-platform low-latency audio backend abstraction layer.
//!
//! The `IAudioBackend` trait decouples the engine from any specific
//! OS audio API.  Concrete implementations:
//!
//! | Backend                  | Platform     | Notes                              |
//! |--------------------------|--------------|-------------------------------------|
//! | `DauxCpalBackend`        | All          | cpal: WASAPI Shared / CoreAudio / ALSA |
//! | `DauxWasapiExclBackend`  | Windows only | Raw WASAPI professional + MMCSS        |
//! | `DauxWdmKsBackend`       | Windows only | WDM-KS low-level driver path        |
//! | `DauxDirectBackend`      | Windows only | DAUx engine, DAUx-KS kernel streaming |
//! | `DauxAsioBackend`        | Windows only | Host supplied by an edition provider |
//! | `DauxMmeBackend`         | Windows only | Legacy MME stub (fallback only)     |
//!
//! Audio engine rule: all backends share `Arc<SharedState>` for meters/transport
//! and receive `EngineCommand` through a `crossbeam_channel::Receiver`.

use std::sync::OnceLock;

#[cfg(all(target_os = "windows", feature = "asio"))]
use crate::error::SphereAudioError;

#[cfg(all(target_os = "windows", feature = "asio"))]
pub mod asio_session;
pub mod cpal_backend;
pub mod render;

#[cfg(target_os = "windows")]
pub mod daux_direct;
#[cfg(target_os = "windows")]
pub mod wasapi_exclusive;
#[cfg(target_os = "windows")]
pub mod wdm_ks;

// ── Backend kind ──────────────────────────────────────────────────────────────

/// Which DAUx audio backend to use for this session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// Platform best-effort via cpal (WASAPI Shared on Win, CoreAudio on Mac, ALSA on Linux).
    #[default]
    Auto,
    /// Windows: WASAPI Shared event-driven (lowest-common-denominator, safe).
    WasapiShared,
    /// Windows: WASAPI Exclusive event-driven (lowest practical latency without ASIO).
    WasapiExclusive,
    /// Windows: WDM-KS low-level driver path (experimental).
    WdmKs,
    /// Windows: the DAUx engine streaming straight to the driver's kernel
    /// pins (DAUx-KS) — covers USB audio class devices WDM-KS cannot.
    DauxDirect,
    /// Windows: Steinberg ASIO driver path supplied by Professional Edition.
    Asio,
    /// macOS: CoreAudio (same as Auto on macOS, explicit selection).
    CoreAudio,
    /// Linux: ALSA PCM (same as Auto on Linux, explicit selection).
    Alsa,
    /// Windows: MME legacy fallback — high latency, maximum compatibility.
    MmeFallback,
}

impl BackendKind {
    pub fn display_name(&self) -> &'static str {
        match self {
            BackendKind::Auto => "Auto",
            BackendKind::WasapiShared => "DAUx WASAPI Shared",
            BackendKind::WasapiExclusive => "DAUx WASAPI Exclusive",
            BackendKind::WdmKs => "DAUx WDM-KS",
            BackendKind::DauxDirect => "DAUx Direct",
            BackendKind::Asio => "DAUx ASIO",
            BackendKind::CoreAudio => "DAUx CoreAudio",
            BackendKind::Alsa => "DAUx ALSA",
            BackendKind::MmeFallback => "DAUx MME (Legacy Fallback)",
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            BackendKind::Auto => "auto",
            BackendKind::WasapiShared => "wasapi-shared",
            BackendKind::WasapiExclusive => "wasapi-exclusive",
            BackendKind::WdmKs => "wdm-ks",
            BackendKind::DauxDirect => "daux-direct",
            BackendKind::Asio => "asio",
            BackendKind::CoreAudio => "coreaudio",
            BackendKind::Alsa => "alsa",
            BackendKind::MmeFallback => "mme",
        }
    }

    pub fn from_id(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "wasapi-shared" | "wasapishared" => BackendKind::WasapiShared,
            "wasapi-exclusive" | "wasapiprofessional" => BackendKind::WasapiExclusive,
            "wdm-ks" | "wdmks" | "wdm_ks" => BackendKind::WdmKs,
            "daux-direct" | "dauxdirect" | "daux-ks" => BackendKind::DauxDirect,
            "asio" | "daux-asio" => BackendKind::Asio,
            "coreaudio" | "core-audio" => BackendKind::CoreAudio,
            "alsa" => BackendKind::Alsa,
            "mme" | "mmefallback" => BackendKind::MmeFallback,
            _ => BackendKind::Auto,
        }
    }

    /// Backends that are actually selectable on the current build target.
    /// `Auto` is always valid everywhere — it falls back to cpal's
    /// platform-default backend (WASAPI Shared / CoreAudio / ALSA).
    pub fn allowed_for_current_platform() -> Vec<BackendKind> {
        #[cfg(target_os = "windows")]
        {
            let mut backends = vec![
                BackendKind::Auto,
                BackendKind::WasapiShared,
                BackendKind::WasapiExclusive,
                BackendKind::WdmKs,
                BackendKind::DauxDirect,
                BackendKind::MmeFallback,
            ];
            if asio_support_enabled() {
                backends.insert(backends.len() - 1, BackendKind::Asio);
            }
            backends
        }
        #[cfg(target_os = "macos")]
        {
            vec![BackendKind::Auto, BackendKind::CoreAudio]
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            vec![BackendKind::Auto, BackendKind::Alsa]
        }
    }

    /// Whether this backend can actually be selected on the current platform.
    pub fn is_allowed_on_current_platform(&self) -> bool {
        Self::allowed_for_current_platform().contains(self)
    }

    /// Normalize a saved/persisted backend id for the current platform. A
    /// backend that isn't valid here (e.g. a Windows id loaded on Linux)
    /// becomes `Auto` in memory — the persisted value on disk is left
    /// untouched unless the user explicitly changes and saves a setting.
    pub fn sanitize_for_current_platform(saved: BackendKind) -> BackendKind {
        if saved.is_allowed_on_current_platform() {
            saved
        } else {
            BackendKind::Auto
        }
    }
}

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration for opening an audio device through the DAUx abstraction.
#[derive(Debug, Clone, Default)]
pub struct DauxDeviceConfig {
    /// Which backend to use.
    pub backend: BackendKind,
    /// Specific output device name/id, or None for the system default.
    pub output_device_id: Option<String>,
    /// Specific input device name/id (for future capture support).
    pub input_device_id: Option<String>,
    /// Requested sample rate (Hz).  None = use device default.
    pub sample_rate: Option<u32>,
    /// Requested buffer size (frames).  None = use driver default.
    pub buffer_size: Option<u32>,
    /// Request MMCSS "Pro Audio" thread priority on Windows.
    pub mmcss_priority: bool,
    /// Safe mode: use larger buffer for stability over latency.
    pub safe_mode: bool,
}

// ── ASIO session capabilities ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsioChannelInfo {
    /// Stable zero-based hardware channel index within the driver.
    pub index: u32,
    /// Driver-provided name, or a deterministic fallback when unavailable.
    pub name: String,
    /// Whether the driver reports the channel active in the current buffer set.
    pub active: bool,
    /// Native sample representation selected for this ASIO direction.
    pub sample_format: String,
    /// Adjacent active pair number when this channel can participate in a
    /// conventional stereo route. `None` never implies that a mono channel is
    /// unavailable.
    pub stereo_pair: Option<u32>,
}

/// Live capabilities of the currently open ASIO session, published by the
/// engine when the stream opens and cleared when it closes. Consumed by device
/// enumeration (channel counts for the active driver) and the Inspector's
/// input-channel model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsioSessionCaps {
    /// asiolist-compatible driver name (the persisted stable id).
    pub driver: String,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub input_channels: u32,
    pub output_channels: u32,
    pub input_channel_info: Vec<AsioChannelInfo>,
    pub output_channel_info: Vec<AsioChannelInfo>,
    /// Driver-reported latencies in samples at `sample_rate` (input, output).
    pub input_latency_samples: u32,
    pub output_latency_samples: u32,
}

// ── Backend runtime status ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct DauxBackendStatus {
    /// Backend identifier string (e.g. "wasapi-shared", "coreaudio").
    pub backend_id: String,
    /// Human-readable backend name.
    pub backend_name: String,
    /// Active output device name.
    pub output_device: Option<String>,
    /// Active sample rate (Hz).
    pub sample_rate: u32,
    /// Active buffer size (frames).
    pub buffer_size: u32,
    /// Estimated round-trip output latency in milliseconds.
    pub estimated_latency_ms: f64,
    /// Number of audio buffer underruns / glitches since stream open.
    pub glitch_count: u64,
    /// Number of xruns (ALSA) / underruns since stream open.
    pub xrun_count: u64,
    /// Last measured callback duration (microseconds).
    pub last_callback_us: u64,
}

// ── Available backend list ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub is_default: bool,
    pub description: String,
}

/// Return all backends available on the current platform.
pub fn list_available_backends() -> Vec<BackendInfo> {
    let mut list = vec![BackendInfo {
        id: "auto".into(),
        name: "Auto".into(),
        available: true,
        is_default: true,
        description: "Platform default (WASAPI Shared / CoreAudio / ALSA; PipeWire via ALSA)"
            .into(),
    }];

    #[cfg(target_os = "windows")]
    {
        list.push(BackendInfo {
            id: "wasapi-shared".into(),
            name: "DAUx WASAPI Shared".into(),
            available: true,
            is_default: false,
            description: "WASAPI Shared event-driven — compatible, ~10-30ms latency".into(),
        });
        list.push(BackendInfo {
            id: "wasapi-exclusive".into(),
            name: "DAUx WASAPI Exclusive".into(),
            available: true,
            is_default: false,
            description: "WASAPI Exclusive + MMCSS — lowest latency, requires device support"
                .into(),
        });
        list.push(BackendInfo {
            id: "wdm-ks".into(),
            name: "DAUx WDM-KS".into(),
            available: true,
            is_default: false,
            description: "WDM-KS low-level Windows driver path — experimental".into(),
        });
        list.push(BackendInfo {
            id: "daux-direct".into(),
            name: "DAUx Direct".into(),
            available: true,
            is_default: false,
            description: "DAUx engine — kernel streaming straight to the driver (USB audio), \
                          bypassing the Windows audio engine"
                .into(),
        });
        if asio_support_enabled() {
            list.push(BackendInfo {
                id: "asio".into(),
                name: "DAUx ASIO".into(),
                available: true,
                is_default: false,
                description: "ASIO native low-latency driver path".into(),
            });
        }
        list.push(BackendInfo {
            id: "mme".into(),
            name: "DAUx MME (Legacy Fallback)".into(),
            available: true,
            is_default: false,
            description: "Windows MME — maximum compatibility, high latency (~100ms+)".into(),
        });
    }

    #[cfg(target_os = "macos")]
    {
        list.push(BackendInfo {
            id: "coreaudio".into(),
            name: "DAUx CoreAudio".into(),
            available: true,
            is_default: false,
            description: "CoreAudio — native macOS low-latency backend".into(),
        });
    }

    #[cfg(target_os = "linux")]
    {
        list.push(BackendInfo {
            id: "alsa".into(),
            name: "DAUx ALSA".into(),
            available: true,
            is_default: false,
            description: "ALSA PCM — native Linux path (works with PipeWire's ALSA plugin)".into(),
        });
    }

    list
}

/// ASIO integration supplied by the separately linked edition crate.
///
/// `current_entitlement` must be a cheap, cached live capability check (for
/// example, an atomic load). The engine calls it at every support, enumeration,
/// and host-acquisition boundary; it must not perform license verification,
/// filesystem access, network access, or other expensive work.
pub struct AsioHostProvider {
    #[allow(dead_code)] // Read only by Windows builds with the `asio` feature.
    host_factory: fn() -> Result<cpal::Host, String>,
    current_entitlement: fn() -> bool,
}

impl AsioHostProvider {
    pub const fn new(
        host_factory: fn() -> Result<cpal::Host, String>,
        current_entitlement: fn() -> bool,
    ) -> Self {
        Self {
            host_factory,
            current_entitlement,
        }
    }

    #[cfg(all(target_os = "windows", feature = "asio"))]
    fn create_host(&self) -> Result<cpal::Host, String> {
        (self.host_factory)()
    }

    fn has_current_entitlement(&self) -> bool {
        (self.current_entitlement)()
    }
}

static ASIO_HOST_PROVIDER: OnceLock<AsioHostProvider> = OnceLock::new();

/// The one ASIO host for the whole process.
///
/// asio-sys tracks "which driver is loaded" *per host instance*, but the
/// underlying ASIO API is process-global (one loaded driver, one global
/// buffer-callback list). A second host instance can therefore load a driver
/// on top of the one that is streaming, and dropping its transient `Driver`
/// runs `ASIOExit` + clears the global callback list — killing the active
/// stream. Memoizing the first successfully created host removes that entire
/// failure class: every enumeration/open/input path shares one `sys::Asio`.
#[cfg(all(target_os = "windows", feature = "asio"))]
struct SharedAsioHost(cpal::Host);

// SAFETY: the stored host is always the ASIO variant produced by the edition
// factory. `cpal::host::asio::Host` wraps `Arc<asio_sys::Asio>`, whose only
// state is `Mutex<Weak<DriverInner>>` — genuinely `Send + Sync`. The platform
// `Host` enum lacks the auto-impl only because other variants are conservative
// about COM thread affinity; we never store those variants here, and all
// driver/stream operations still happen on the control thread.
#[cfg(all(target_os = "windows", feature = "asio"))]
unsafe impl Send for SharedAsioHost {}
#[cfg(all(target_os = "windows", feature = "asio"))]
unsafe impl Sync for SharedAsioHost {}

#[cfg(all(target_os = "windows", feature = "asio"))]
static ASIO_SHARED_HOST: OnceLock<SharedAsioHost> = OnceLock::new();

/// Register the ASIO provider supplied by the separately linked edition crate.
pub fn register_asio_host_provider(provider: AsioHostProvider) -> Result<(), String> {
    ASIO_HOST_PROVIDER
        .set(provider)
        .map_err(|_| "ASIO host provider is already registered".to_string())
}

fn asio_support_enabled_for(provider: Option<&AsioHostProvider>) -> bool {
    cfg!(target_os = "windows")
        && cfg!(feature = "asio")
        && provider.is_some_and(AsioHostProvider::has_current_entitlement)
}

#[cfg(all(target_os = "windows", feature = "asio"))]
pub(crate) fn asio_host() -> Result<&'static cpal::Host, SphereAudioError> {
    let provider = ASIO_HOST_PROVIDER.get().ok_or_else(|| {
        SphereAudioError::BackendUnavailable(
            "DAUx ASIO requires a Futureboard Professional Edition provider".into(),
        )
    })?;
    let require_entitlement = || {
        if provider.has_current_entitlement() {
            Ok(())
        } else {
            Err(SphereAudioError::BackendUnavailable(
                "DAUx ASIO requires a current Professional Edition ASIO entitlement".into(),
            ))
        }
    };

    // Re-check before returning even a memoized host. Registration and host
    // creation are process-lifetime operations; entitlement is deliberately
    // live and may become false after either one.
    require_entitlement()?;
    if let Some(host) = ASIO_SHARED_HOST.get() {
        return Ok(&host.0);
    }

    let host = provider
        .create_host()
        .map_err(SphereAudioError::BackendUnavailable)?;
    require_entitlement()?;

    // A losing racer's fresh host is dropped unused — harmless, no driver has
    // been loaded through it.
    let host = &ASIO_SHARED_HOST.get_or_init(|| SharedAsioHost(host)).0;
    require_entitlement()?;
    Ok(host)
}

/// Whether ASIO may currently be offered or enumerated.
///
/// Support requires all three conditions: a Windows target, the engine's
/// `asio` feature, and a registered provider whose live entitlement is true.
pub fn asio_support_enabled() -> bool {
    asio_support_enabled_for(ASIO_HOST_PROVIDER.get())
}

#[cfg(test)]
mod tests {
    use super::{AsioHostProvider, BackendKind};

    #[cfg(all(target_os = "windows", feature = "asio"))]
    use std::sync::atomic::{AtomicBool, Ordering};

    #[cfg(all(target_os = "windows", feature = "asio"))]
    static TEST_ASIO_ENTITLEMENT: AtomicBool = AtomicBool::new(false);

    #[cfg(all(target_os = "windows", feature = "asio"))]
    fn test_asio_host() -> Result<cpal::Host, String> {
        cpal::host_from_id(cpal::HostId::Asio)
            .map_err(|error| format!("ASIO host initialization failed: {error}"))
    }

    #[cfg(all(target_os = "windows", feature = "asio"))]
    fn test_asio_entitlement() -> bool {
        TEST_ASIO_ENTITLEMENT.load(Ordering::Acquire)
    }

    #[test]
    fn daux_direct_backend_id_round_trips() {
        assert_eq!(BackendKind::from_id("daux-direct"), BackendKind::DauxDirect);
        assert_eq!(BackendKind::DauxDirect.id(), "daux-direct");
        assert_eq!(
            BackendKind::DauxDirect.is_allowed_on_current_platform(),
            cfg!(target_os = "windows")
        );
    }

    #[test]
    fn asio_backend_id_round_trips() {
        assert_eq!(BackendKind::from_id("asio"), BackendKind::Asio);
        assert_eq!(BackendKind::Asio.id(), "asio");
    }

    #[test]
    fn unavailable_asio_setting_sanitizes_to_auto() {
        if !super::asio_support_enabled() {
            assert_eq!(
                BackendKind::sanitize_for_current_platform(BackendKind::Asio),
                BackendKind::Auto
            );
        }
    }

    #[cfg(all(target_os = "windows", feature = "asio"))]
    #[test]
    fn asio_host_rechecks_true_to_false_entitlement() {
        TEST_ASIO_ENTITLEMENT.store(true, Ordering::Release);
        super::register_asio_host_provider(AsioHostProvider::new(
            test_asio_host,
            test_asio_entitlement,
        ))
        .expect("test ASIO provider should register once");

        assert!(super::asio_support_enabled());
        super::asio_host().expect("true entitlement should acquire the shared ASIO host");

        TEST_ASIO_ENTITLEMENT.store(false, Ordering::Release);

        assert!(!super::asio_support_enabled());
        assert!(!BackendKind::allowed_for_current_platform().contains(&BackendKind::Asio));
        assert!(
            super::asio_host().is_err(),
            "a cached host must not bypass a revoked entitlement"
        );
    }

    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    #[test]
    fn asio_support_stays_disabled_without_windows_asio_feature() {
        fn unused_host_factory() -> Result<cpal::Host, String> {
            Err("host factory must not run".into())
        }
        fn entitled() -> bool {
            true
        }

        let provider = AsioHostProvider::new(unused_host_factory, entitled);
        assert!(!super::asio_support_enabled_for(Some(&provider)));
    }
}
