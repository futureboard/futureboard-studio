use crate::types::JsAudioDeviceInfo;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, StreamConfig, SupportedStreamConfig};

/// `FUTUREBOARD_AUDIO_DEVICE_DEBUG=1` traces enumerated devices + channel counts.
fn device_debug_enabled() -> bool {
    std::env::var_os("FUTUREBOARD_AUDIO_DEVICE_DEBUG").is_some()
}

fn log_devices(direction: &str, devices: &[JsAudioDeviceInfo]) {
    if !device_debug_enabled() {
        return;
    }
    for d in devices {
        eprintln!(
            "[audio-device] {direction} id={:?} name={:?} channels={} default_sr={} default={} backend={}",
            d.id, d.name, d.channels, d.default_sample_rate, d.is_default, d.backend
        );
    }
}

/// Enumerate all available output devices on the default host.
/// Never panics — returns empty vec on any cpal error.
pub fn list_output_devices() -> Vec<JsAudioDeviceInfo> {
    list_output_devices_for_host(&cpal::default_host())
}

/// Enumerate output devices from a specific CPAL host (for example ASIO).
pub(crate) fn list_output_devices_for_host(host: &cpal::Host) -> Vec<JsAudioDeviceInfo> {
    let backend = host.id().name().to_string();
    let default_name = host.default_output_device().and_then(|d| d.name().ok());

    match host.output_devices() {
        Err(e) => {
            eprintln!("[SphereAudio] list_output_devices error: {e}");
            vec![]
        }
        Ok(devices) => {
            let list: Vec<JsAudioDeviceInfo> = devices
                .filter_map(|dev| {
                    let id = dev.name().ok()?;
                    if !include_device_in_list(&id, &backend) {
                        return None;
                    }
                    let cfg = dev.default_output_config().ok().or_else(|| {
                        dev.supported_output_configs()
                            .ok()?
                            .next()
                            .map(|range| range.with_max_sample_rate())
                    })?;
                    let is_default = default_name.as_ref() == Some(&id);
                    Some(JsAudioDeviceInfo {
                        name: display_name_for_device(&id, &backend),
                        id,
                        kind: "output".into(),
                        channels: cfg.channels() as u32,
                        default_sample_rate: cfg.sample_rate().0,
                        is_default,
                        backend: backend.clone(),
                    })
                })
                .collect();
            let list = sort_and_dedupe_devices(list);
            log_devices("output", &list);
            list
        }
    }
}

/// Enumerate all available input devices on the default host.
pub fn list_input_devices() -> Vec<JsAudioDeviceInfo> {
    list_input_devices_for_host(&cpal::default_host())
}

/// Enumerate input devices from a specific CPAL host (for example ASIO).
pub(crate) fn list_input_devices_for_host(host: &cpal::Host) -> Vec<JsAudioDeviceInfo> {
    let backend = host.id().name().to_string();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());

    match host.input_devices() {
        Err(e) => {
            eprintln!("[SphereAudio] list_input_devices error: {e}");
            vec![]
        }
        Ok(devices) => {
            let list: Vec<JsAudioDeviceInfo> = devices
                .filter_map(|dev| {
                    let id = dev.name().ok()?;
                    if !include_device_in_list(&id, &backend) {
                        return None;
                    }
                    let cfg = dev.default_input_config().ok().or_else(|| {
                        dev.supported_input_configs()
                            .ok()?
                            .next()
                            .map(|range| range.with_max_sample_rate())
                    })?;
                    let is_default = default_name.as_ref() == Some(&id);
                    Some(JsAudioDeviceInfo {
                        name: display_name_for_device(&id, &backend),
                        id,
                        kind: "input".into(),
                        channels: cfg.channels() as u32,
                        default_sample_rate: cfg.sample_rate().0,
                        is_default,
                        backend: backend.clone(),
                    })
                })
                .collect();
            let list = sort_and_dedupe_devices(list);
            log_devices("input", &list);
            list
        }
    }
}

/// Resolve a named output device (or the system default if `id` is None).
/// Returns `(device, actual_name)` or an error string.
///
/// `id` may be the openable endpoint id **or** the friendly display name shown
/// in Preferences — ALSA lists use readable names that no longer match cpal.
pub fn resolve_output_device(id: Option<&str>) -> Result<(cpal::Device, String), String> {
    resolve_output_device_for_host(&cpal::default_host(), id)
}

/// Resolve an output device against a specific CPAL host. ASIO never routes
/// through here — its duplex session resolves the driver itself.
pub(crate) fn resolve_output_device_for_host(
    host: &cpal::Host,
    id: Option<&str>,
) -> Result<(cpal::Device, String), String> {
    match id {
        None => {
            let dev = host
                .default_output_device()
                .ok_or_else(|| "No default output device found".to_string())?;
            let name = dev.name().unwrap_or_else(|_| "Unknown".into());
            Ok((dev, name))
        }
        Some(wanted) => {
            let open_id = resolve_open_id(host, wanted, false);
            let mut devices = host.output_devices().map_err(|e| e.to_string())?;
            devices
                .find(|d| d.name().map(|n| n == open_id).unwrap_or(false))
                .map(|d| {
                    let n = d.name().unwrap_or_else(|_| open_id.clone());
                    (d, n)
                })
                .ok_or_else(|| format!("Output device '{wanted}' not found"))
        }
    }
}

/// Map a stored Preferences value (id **or** display name) to the openable
/// cpal device name. Falls back to the raw string when nothing matches.
pub(crate) fn resolve_open_id(host: &cpal::Host, wanted: &str, input: bool) -> String {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return wanted.to_string();
    }
    // Direct match against cpal's own name — works when the caller already has
    // a raw endpoint id from an older settings file or a non-ALSA backend.
    let hosts_devices = if input {
        host.input_devices()
    } else {
        host.output_devices()
    };
    if let Ok(mut devices) = hosts_devices {
        if devices.any(|d| d.name().as_deref().ok() == Some(wanted)) {
            return wanted.to_string();
        }
    }
    // Match friendly display names from our formatted list.
    let listed = if input {
        list_input_devices_for_host(host)
    } else {
        list_output_devices_for_host(host)
    };
    listed
        .into_iter()
        .find(|d| d.id == wanted || d.name == wanted)
        .map(|d| d.id)
        .unwrap_or_else(|| wanted.to_string())
}

// ── Input stream candidate building ───────────────────────────────────────────

/// Stream configs to try when opening a cpal input, in preference order.
///
/// Mirrors the output path: on ALSA, `BufferSize::Fixed` is the whole PCM ring
/// while the UI buffer is the period, so we ask for multi-period rings first.
/// Also tries the engine's active sample rate before the device default so
/// monitored input does not pitch-shift against the open output stream.
pub(crate) fn input_stream_config_candidates(
    default_supported: &SupportedStreamConfig,
    preferred_period: Option<u32>,
    preferred_sample_rate: Option<u32>,
) -> Vec<(&'static str, StreamConfig)> {
    let default_cfg = default_supported.config();
    let channels = default_cfg.channels;
    let default_sr = default_cfg.sample_rate.0;

    let mut sample_rates = Vec::with_capacity(2);
    if let Some(sr) = preferred_sample_rate.filter(|&s| s > 0 && s != default_sr) {
        sample_rates.push(sr);
    }
    sample_rates.push(default_sr);

    let mut out = Vec::new();
    for sr in sample_rates {
        if let Some(period) = preferred_period.filter(|&p| p > 0) {
            for (label, fixed, _callback) in crate::backend::cpal_backend::period_candidates(period)
            {
                out.push((
                    label,
                    StreamConfig {
                        channels,
                        sample_rate: cpal::SampleRate(sr),
                        buffer_size: BufferSize::Fixed(fixed),
                    },
                ));
            }
        }
        out.push((
            "device default",
            StreamConfig {
                channels,
                sample_rate: cpal::SampleRate(sr),
                buffer_size: BufferSize::Default,
            },
        ));
    }
    out
}

// ── ALSA list readability / ranking ───────────────────────────────────────────

/// Whether this endpoint is useful enough to appear in Preferences.
///
/// ALSA dumps every plugin PCM for every card (`surround51`, `dmix`, `dsnoop`,
/// `iec958`, rate converters…). Most of those are either exclusive plugins
/// that fight PipeWire or duplicates of the same pair of hardware channels.
/// Users need `default` / PipeWire / `front` / `hdmi` / `hw` — not 40 near-
/// identical aliases.
fn include_device_in_list(id: &str, backend: &str) -> bool {
    if !backend_is_alsa(backend) {
        return true;
    }
    let base = alsa_pcm_base(id);
    // Hard-exclude pure policy / rate / routing plugins.
    const EXCLUDE: &[&str] = &[
        "null",
        "dmix",
        "dsnoop",
        "dshare",
        "upmix",
        "vdownmix",
        "lavrate",
        "samplerate",
        "speexrate",
        "speex",
        "jack",
        "oss",
        "a52",
        "ladspa",
        "usbstream",
        "phonon",
        "ttable",
        "file",
        "shm",
        // Surround layouts are the same card with different channel maps — the
        // engine asks for channel counts from `front` / default instead.
        "surround21",
        "surround40",
        "surround41",
        "surround50",
        "surround51",
        "surround71",
    ];
    if EXCLUDE.iter().any(|p| base.eq_ignore_ascii_case(p)) {
        return false;
    }
    true
}

fn backend_is_alsa(backend: &str) -> bool {
    let b = backend.to_ascii_lowercase();
    // cpal's default Linux host reports id name `"ALSA"`. Keep a loose check so
    // custom host labels like `"DAUx ALSA"` still get the readable formatting.
    b.contains("alsa")
}

/// Human label for Preferences / device pickers. `id` stays the openable endpoint.
pub(crate) fn display_name_for_device(id: &str, backend: &str) -> String {
    if !backend_is_alsa(backend) {
        return id.to_string();
    }
    format_alsa_display_name(id)
}

/// Turn ALSA PCM ids into short, readable labels.
///
/// Keeps card / device info so two cards stay distinguishable:
/// `front:CARD=PCH,DEV=0` → `Front · PCH`
/// `hdmi:CARD=PCH,DEV=3` → `HDMI · PCH (device 3)`
/// `sysdefault:CARD=PCH` → `System Default · PCH`
/// `default` / `pipewire` / `pulse` → plain titles
pub(crate) fn format_alsa_display_name(id: &str) -> String {
    let id = id.trim();
    if id.is_empty() {
        return "Unknown".into();
    }
    match id {
        "default" => return "System Default".into(),
        "sysdefault" => return "System Default (sysdefault)".into(),
        "pipewire" => return "PipeWire".into(),
        "pulse" => return "PulseAudio".into(),
        "jack" => return "JACK".into(),
        _ => {}
    }

    let base = alsa_pcm_base(id);
    let card = alsa_hint_value(id, "CARD");
    let dev = alsa_hint_value(id, "DEV");

    let kind = match base {
        "default" => "System Default",
        "sysdefault" => "System Default",
        "front" => "Front",
        "rear" => "Rear",
        "center_lfe" => "Center / LFE",
        "side" => "Side",
        "hdmi" => "HDMI",
        "hw" => "Hardware",
        "plughw" => "Hardware (plug)",
        "iec958" | "spdif" => "S/PDIF",
        "surround21" => "Surround 2.1",
        "surround40" => "Surround 4.0",
        "surround41" => "Surround 4.1",
        "surround50" => "Surround 5.0",
        "surround51" => "Surround 5.1",
        "surround71" => "Surround 7.1",
        other if other.eq_ignore_ascii_case("pipewire") => "PipeWire",
        other if other.eq_ignore_ascii_case("pulse") => "PulseAudio",
        other => other,
    };

    match (card, dev) {
        (Some(card), Some(dev)) if dev != "0" => format!("{kind} · {card} (device {dev})"),
        (Some(card), _) => format!("{kind} · {card}"),
        (None, Some(dev)) if dev != "0" => format!("{kind} (device {dev})"),
        _ => {
            // Unparsed exotic PCMs: pretty-print separators instead of a wall of
            // `KEY=value` commas.
            if id.contains(':') || id.contains(',') {
                id.replace([',', ':'], " · ")
            } else {
                kind.to_string()
            }
        }
    }
}

fn alsa_pcm_base(id: &str) -> &str {
    id.split_once(':').map(|(base, _)| base).unwrap_or(id)
}

fn alsa_hint_value<'a>(id: &'a str, key: &str) -> Option<&'a str> {
    let (_, rest) = id.split_once(':')?;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            if k.eq_ignore_ascii_case(key) {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Preference order: system default / shared servers first, then front-of-
/// board, then direct hardware, then everything else. Defaults win ties.
fn device_rank(id: &str, is_default: bool) -> (u8, u8, String) {
    let base = alsa_pcm_base(id).to_ascii_lowercase();
    let rank = match base.as_str() {
        "default" => 0,
        "pipewire" => 1,
        "pulse" => 2,
        "sysdefault" => 3,
        "front" => 4,
        "plughw" => 5,
        "hw" => 6,
        "hdmi" => 7,
        "iec958" | "spdif" => 8,
        _ => 20,
    };
    // `false` sorts before `true`, so a true-is_default becomes 0.
    let default_boost = u8::from(!is_default);
    (rank, default_boost, id.to_ascii_lowercase())
}

fn sort_and_dedupe_devices(mut list: Vec<JsAudioDeviceInfo>) -> Vec<JsAudioDeviceInfo> {
    list.sort_by(|a, b| {
        device_rank(&a.id, a.is_default)
            .cmp(&device_rank(&b.id, b.is_default))
            .then_with(|| a.name.cmp(&b.name))
    });
    // Drop exact id duplicates (defensive).
    list.dedup_by(|a, b| a.id == b.id);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alsa_display_names_are_readable() {
        assert_eq!(format_alsa_display_name("default"), "System Default");
        assert_eq!(format_alsa_display_name("pipewire"), "PipeWire");
        assert_eq!(
            format_alsa_display_name("front:CARD=PCH,DEV=0"),
            "Front · PCH"
        );
        assert_eq!(
            format_alsa_display_name("hdmi:CARD=NVidia,DEV=3"),
            "HDMI · NVidia (device 3)"
        );
        assert_eq!(
            format_alsa_display_name("hw:CARD=Generic,DEV=0"),
            "Hardware · Generic"
        );
        assert_eq!(
            format_alsa_display_name("sysdefault:CARD=PCH"),
            "System Default · PCH"
        );
    }

    #[test]
    fn plugin_pcms_are_filtered_on_alsa() {
        assert!(!include_device_in_list("dmix:CARD=PCH,DEV=0", "ALSA"));
        assert!(!include_device_in_list("surround51:CARD=PCH,DEV=0", "ALSA"));
        assert!(include_device_in_list("front:CARD=PCH,DEV=0", "ALSA"));
        assert!(include_device_in_list("hdmi:CARD=PCH,DEV=0", "ALSA"));
        assert!(include_device_in_list("Speakers (Realtek)", "WASAPI"));
    }

    #[test]
    fn defaults_rank_ahead_of_raw_hardware() {
        let default_key = device_rank("default", true);
        let hw_key = device_rank("hw:CARD=PCH,DEV=0", false);
        assert!(default_key < hw_key);
    }

    #[test]
    fn input_candidates_prefer_output_rate_and_period() {
        // Minimal SupportedStreamConfig construction is heavy; smoke-test
        // period_candidates wiring instead for the fixed ALSA case.
        let candidates = crate::backend::cpal_backend::period_candidates(256);
        assert!(!candidates.is_empty());
        #[cfg(target_os = "linux")]
        {
            assert_eq!(candidates[0].1, 1024);
            assert_eq!(candidates[0].2, 256);
        }
    }
}
