use anyhow::{Context, Result};
use itertools::Itertools;
use util::ResultExt;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
            D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Dxgi::{
            CreateDXGIFactory2, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_DEBUG,
            DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
            DXGI_GPU_PREFERENCE_MINIMUM_POWER, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory6,
        },
    },
};
use windows::core::Interface;

pub(crate) fn try_to_recover_from_device_lost<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    (0..5)
        .map(|i| {
            if i > 0 {
                // Add a small delay before retrying
                std::thread::sleep(std::time::Duration::from_millis(100 + i * 10));
            }
            f()
        })
        .find_or_last(Result::is_ok)
        .unwrap()
        .context("DirectXRenderer failed to recover from lost device after multiple attempts")
}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let debug_layer_available = check_debug_layer_available();
        log::info!(
            "D3D11 startup device selection begin debug_layer_available={debug_layer_available} bgra_support=true force_warp={}",
            force_warp_requested()
        );
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;
        let (adapter, device, device_context, feature_level) = if force_warp_requested() {
            get_warp_adapter(&dxgi_factory, debug_layer_available)
                .context("Creating requested WARP D3D11 device")?
        } else {
            match get_adapter(&dxgi_factory, debug_layer_available) {
                Ok(devices) => devices,
                Err(hardware_error) => {
                    log::error!(
                        "hardware D3D11 adapter selection failed; retrying with WARP: {hardware_error:#}"
                    );
                    get_warp_adapter(&dxgi_factory, debug_layer_available)
                        .context("Hardware D3D11 failed and WARP fallback also failed")?
                }
            }
        };
        match feature_level {
            D3D_FEATURE_LEVEL_11_1 => {
                log::info!("Created device with Direct3D 11.1 feature level.")
            }
            D3D_FEATURE_LEVEL_11_0 => {
                log::info!("Created device with Direct3D 11.0 feature level.")
            }
            D3D_FEATURE_LEVEL_10_1 => {
                log::info!("Created device with Direct3D 10.1 feature level.")
            }
            _ => unreachable!(),
        }
        log::info!(
            "D3D11 device selected adapter={} feature_level={feature_level:?} device_flags=BGRA_SUPPORT{}",
            adapter_identity(&adapter),
            if debug_layer_available { "|DEBUG" } else { "" }
        );

        Ok(Self {
            adapter,
            dxgi_factory,
            device,
            device_context,
        })
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        let available = unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some();
        log::info!("D3D11 debug layer state available={available}");
        available
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory6> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

#[inline]
fn get_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<(
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
)> {
    if let Some(found) =
        get_preferred_adapter(dxgi_factory, debug_layer_available, &adapter_preference())
    {
        return Ok(found);
    }

    let mut adapter_index = 0;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { dxgi_factory.EnumAdapters(adapter_index) } {
            Ok(adapter) => adapter.cast()?,
            Err(error) => {
                log::info!(
                    "DXGI adapter enumeration ended at index={adapter_index} HRESULT=0x{:08X} message={}",
                    error.code().0 as u32,
                    error.message()
                );
                break;
            }
        };
        log_adapter(adapter_index, &adapter);
        // Check to see whether the adapter supports Direct3D 11 and create
        // the device if it does.
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        match get_device(
            &adapter,
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        ) {
            Ok(device) => {
                log::info!("DXGI adapter selected index={adapter_index}");
                return Ok((adapter, device, context.unwrap(), feature_level));
            }
            Err(error) => log::warn!(
                "DXGI adapter rejected index={adapter_index} adapter={} error={error:#}",
                adapter_identity(&adapter)
            ),
        }
        adapter_index += 1;
    }

    Err(anyhow::anyhow!(
        "No enumerated DXGI adapter created a supported D3D11 device"
    ))
}

/// Which adapter the user asked for, from `FUTUREBOARD_GPU_ADAPTER`. The app
/// sets it from Preferences → Performance → GPU Device before the platform is
/// created; setting it by hand overrides the preference.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AdapterPreference {
    /// The OS default (DXGI enumeration order) — the historical behaviour.
    Default,
    HighPerformance,
    LowPower,
    /// A specific adapter, by PCI vendor/device id (name as a tiebreak).
    Device {
        vendor: u32,
        device: u32,
        name: String,
    },
}

fn adapter_preference() -> AdapterPreference {
    std::env::var("FUTUREBOARD_GPU_ADAPTER")
        .map(|value| parse_adapter_preference(&value))
        .unwrap_or(AdapterPreference::Default)
}

fn parse_adapter_preference(value: &str) -> AdapterPreference {
    let value = value.trim();
    match value.to_ascii_lowercase().as_str() {
        "" | "auto" | "default" => return AdapterPreference::Default,
        "high-performance" | "highperformance" | "high" | "discrete" | "dgpu" => {
            return AdapterPreference::HighPerformance;
        }
        "low-power" | "lowpower" | "low" | "integrated" | "igpu" => {
            return AdapterPreference::LowPower;
        }
        _ => {}
    }
    let (mut vendor, mut device, mut name) = (None, None, String::new());
    for part in value.split(';') {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let number = || {
            let raw = raw.trim();
            let hex = raw.trim_start_matches("0x").trim_start_matches("0X");
            u32::from_str_radix(hex, 16).ok()
        };
        match key.trim() {
            "vendor" => vendor = number(),
            "device" => device = number(),
            "name" => name = raw.trim().to_string(),
            _ => {}
        }
    }
    match (vendor, device) {
        (Some(vendor), Some(device)) => AdapterPreference::Device {
            vendor,
            device,
            name,
        },
        _ => AdapterPreference::Default,
    }
}

/// The adapter the preference names, if it exists and creates a usable
/// device. `None` falls back to the default enumeration, so a stale choice
/// (GPU removed, driver broken) never stops the app from starting.
///
/// Enumerates by GPU preference rather than plain `EnumAdapters`: on a hybrid
/// laptop index 0 is the integrated GPU the display is wired to, which is why
/// "use the discrete GPU" used to be impossible to get.
fn get_preferred_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
    preference: &AdapterPreference,
) -> Option<(
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
)> {
    let order: DXGI_GPU_PREFERENCE = match preference {
        AdapterPreference::Default => return None,
        AdapterPreference::LowPower => DXGI_GPU_PREFERENCE_MINIMUM_POWER,
        AdapterPreference::HighPerformance | AdapterPreference::Device { .. } => {
            DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE
        }
    };
    log::info!("DXGI adapter preference {preference:?}");
    let mut index = 0;
    loop {
        let adapter: IDXGIAdapter1 =
            match unsafe { dxgi_factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(index, order) }
            {
                Ok(adapter) => adapter,
                Err(_) => break,
            };
        index += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        // WARP is never what "this GPU" means.
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        if let AdapterPreference::Device { vendor, device, .. } = preference {
            if desc.VendorId != *vendor || desc.DeviceId != *device {
                continue;
            }
        }
        log_adapter(format!("preferred#{}", index - 1), &adapter);
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        match get_device(
            &adapter,
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        ) {
            Ok(device) => {
                log::info!(
                    "DXGI preferred adapter selected adapter={}",
                    adapter_identity(&adapter)
                );
                return Some((adapter, device, context?, feature_level));
            }
            Err(error) => log::warn!(
                "DXGI preferred adapter rejected adapter={} error={error:#}",
                adapter_identity(&adapter)
            ),
        }
    }
    log::warn!("DXGI adapter preference {preference:?} not satisfied; using the default adapter");
    None
}

#[cfg(test)]
mod adapter_preference_tests {
    use super::*;

    #[test]
    fn parses_what_the_app_writes() {
        assert_eq!(
            parse_adapter_preference("vendor=0x10de;device=0x2520;name=NVIDIA GeForce RTX 3060"),
            AdapterPreference::Device {
                vendor: 0x10de,
                device: 0x2520,
                name: "NVIDIA GeForce RTX 3060".to_string(),
            }
        );
        assert_eq!(
            parse_adapter_preference("discrete"),
            AdapterPreference::HighPerformance
        );
        assert_eq!(parse_adapter_preference(""), AdapterPreference::Default);
        assert_eq!(
            parse_adapter_preference("vendor=zz"),
            AdapterPreference::Default
        );
    }
}

fn get_warp_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<(
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
)> {
    let warp_adapter: IDXGIAdapter = unsafe { dxgi_factory.EnumWarpAdapter::<IDXGIAdapter>()? };
    let adapter: IDXGIAdapter1 = warp_adapter.cast()?;
    log_adapter("WARP", &adapter);
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    let device = get_device(
        &adapter,
        Some(&mut context),
        Some(&mut feature_level),
        debug_layer_available,
    )
    .context("Creating WARP D3D11 device")?;
    log::warn!(
        "Using WARP software D3D11 device adapter={}",
        adapter_identity(&adapter)
    );
    Ok((adapter, device, context.unwrap(), feature_level))
}

pub(crate) fn adapter_identity(adapter: &IDXGIAdapter1) -> String {
    match unsafe { adapter.GetDesc1() } {
        Ok(desc) => {
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            format!(
                "{name:?} vendor=0x{:04X} device=0x{:04X} luid={:08X}:{:08X}",
                desc.VendorId,
                desc.DeviceId,
                desc.AdapterLuid.HighPart as u32,
                desc.AdapterLuid.LowPart
            )
        }
        Err(error) => format!(
            "<GetDesc1 failed HRESULT=0x{:08X}: {}>",
            error.code().0 as u32,
            error.message()
        ),
    }
}

fn log_adapter(index: impl std::fmt::Display, adapter: &IDXGIAdapter1) {
    match unsafe { adapter.GetDesc1() } {
        Ok(desc) => {
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            log::info!(
                "DXGI adapter index={index} name={name:?} vendor_id=0x{:04X} device_id=0x{:04X} luid={:08X}:{:08X} dedicated_vram={} shared_memory={} flags=0x{:X}",
                desc.VendorId,
                desc.DeviceId,
                desc.AdapterLuid.HighPart as u32,
                desc.AdapterLuid.LowPart,
                desc.DedicatedVideoMemory,
                desc.SharedSystemMemory,
                desc.Flags
            );
        }
        Err(error) => log::error!(
            "DXGI adapter index={index} GetDesc1 failed HRESULT=0x{:08X} message={}",
            error.code().0 as u32,
            error.message()
        ),
    }
}

fn force_warp_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .any(|arg| arg.to_string_lossy().eq_ignore_ascii_case("--force-warp"))
        || std::env::var_os("FUTUREBOARD_FORCE_WARP").is_some()
}

#[inline]
fn get_device(
    adapter: &IDXGIAdapter1,
    context: Option<*mut Option<ID3D11DeviceContext>>,
    feature_level: Option<*mut D3D_FEATURE_LEVEL>,
    debug_layer_available: bool,
) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    log::info!(
        "D3D11CreateDevice begin adapter={} driver_type=UNKNOWN flags=0x{:X} bgra_support=true debug_layer={} feature_levels=[11_1,11_0,10_1]",
        adapter_identity(adapter),
        device_flags.0,
        debug_layer_available
    );
    if let Err(error) = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            feature_level,
            context,
        )
    } {
        log::error!(
            "D3D11CreateDevice failed HRESULT=0x{:08X} message={} adapter={}",
            error.code().0 as u32,
            error.message(),
            adapter_identity(adapter)
        );
        return Err(error).context("D3D11CreateDevice");
    }
    let device = device.unwrap();
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")?;
    }
    log::info!(
        "D3D11CreateDevice OK adapter={} feature_level={:?} flags=0x{:X} structured_buffers={}",
        adapter_identity(adapter),
        feature_level.map(|value| unsafe { *value }),
        device_flags.0,
        data.ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
            .as_bool()
    );
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(device)
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}
