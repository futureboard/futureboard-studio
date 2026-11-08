use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use crate::scan::isolation::plugin_info_from_descriptor;
use crate::scan::types::{PluginDescriptor, PluginScanError, PluginScanFormat, PluginScanStatus};
use crate::scanner::{stable_id_for_au, NativePluginInfo};

#[repr(C)]
struct SpherePluginHostString {
    data: *const c_char,
    len: u64,
}

extern "C" {
    fn sphere_au_scan_json() -> SpherePluginHostString;
    fn sphere_au_validate_component_json(component_id: *const c_char) -> SpherePluginHostString;
    fn sphere_plugin_host_free_string(value: SpherePluginHostString);
}

pub fn scan_audio_units(_validate: bool) -> Result<Vec<PluginDescriptor>, PluginScanError> {
    if !cfg!(target_os = "macos") {
        return Err(PluginScanError::UnsupportedPlatform);
    }

    crate::scan::log::scan_start(PluginScanFormat::AudioUnit);
    let native = unsafe { sphere_au_scan_json() };
    if native.data.is_null() {
        return Err(PluginScanError::AudioUnitEnumerationFailed(
            "native scanner returned null".into(),
        ));
    }

    let json = unsafe { CStr::from_ptr(native.data) }
        .to_string_lossy()
        .into_owned();
    unsafe { sphere_plugin_host_free_string(native) };

    let scanned: Vec<NativePluginInfo> = serde_json::from_str(&json).map_err(|error| {
        PluginScanError::AudioUnitMetadataFailed(format!("invalid AU JSON: {error}"))
    })?;

    Ok(scanned
        .into_iter()
        .filter_map(|entry| native_au_to_descriptor(entry).ok())
        .collect())
}

pub fn validate_au_component(component_id: &str) -> Result<bool, PluginScanError> {
    if !cfg!(target_os = "macos") {
        return Err(PluginScanError::UnsupportedPlatform);
    }
    let c_id = CString::new(component_id)
        .map_err(|_| PluginScanError::InvalidComponent(component_id.to_string()))?;
    let native = unsafe { sphere_au_validate_component_json(c_id.as_ptr()) };
    if native.data.is_null() {
        // The native side only returns null when it could not allocate the
        // result; that is a validator failure, not a component verdict.
        return Err(PluginScanError::AudioUnitMetadataFailed(
            "native validator returned null".into(),
        ));
    }
    let json = unsafe { CStr::from_ptr(native.data) }
        .to_string_lossy()
        .into_owned();
    unsafe { sphere_plugin_host_free_string(native) };
    Ok(json.contains("\"ok\":true"))
}

fn native_au_to_descriptor(entry: NativePluginInfo) -> Result<PluginDescriptor, PluginScanError> {
    if entry.name.trim().is_empty() {
        return Err(PluginScanError::NullComponentName);
    }

    let class_id = entry.class_id.clone().filter(|value| !value.is_empty());
    let id_source = class_id
        .clone()
        .or_else(|| Some(entry.path.clone()))
        .unwrap_or_else(|| entry.name.clone());
    let id = stable_id_for_au(&id_source);
    let category = if entry.category.is_empty() {
        "AudioUnit".to_string()
    } else {
        entry.category
    };
    let is_instrument =
        category.eq_ignore_ascii_case("instrument") || category.eq_ignore_ascii_case("generator");
    Ok(PluginDescriptor {
        id,
        format: "AU".into(),
        name: entry.name,
        vendor: if entry.vendor.is_empty() {
            "Unknown Vendor".into()
        } else {
            entry.vendor
        },
        version: entry.version,
        path_or_identifier: entry.path,
        category,
        // ARA hosting is VST3-only in Futureboard; an AU row never claims it.
        is_ara: false,
        is_instrument,
        is_effect: !is_instrument,
        scan_status: if entry.sdk_metadata_loaded {
            PluginScanStatus::Success
        } else {
            PluginScanStatus::Failed
        },
        error_message: None,
        class_id,
        sub_categories: entry.sub_categories,
        sdk_metadata_loaded: entry.sdk_metadata_loaded,
    })
}

pub fn descriptor_to_plugin_info(descriptor: &PluginDescriptor) -> crate::types::PluginInfo {
    plugin_info_from_descriptor(descriptor)
}
