//! TONE3000 catalog fetch for Rodhareist NAM A2.
//!
//! Native owns the HTTP client and any API credential. The CEF editor never
//! sees a token: it asks this module to **search tones** and **download a
//! NAM A2 file**, then loads the bytes through the existing `.nam` path.
//!
//! No OAuth and no per-user login. Authenticate with a TONE3000 API key
//! (`t3k_cs_…` secret, or an equivalent Bearer credential). The key is baked
//! at build time from `FUTUREBOARD_TONE3000_API_KEY` / `TONE3000_API_KEY` or
//! the workspace `.env` (`option_env!`), and a process env of the same names
//! still overrides it at runtime. Direct API calls use
//! `Authorization: Bearer <key>` as TONE3000 documents for server-side
//! fetch. The key never crosses the CEF bridge.

use std::time::Duration;

use serde::{Deserialize, Serialize};

const API_BASE: &str = "https://www.tone3000.com";
const API_TIMEOUT_SECS: u64 = 20;
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;
const NAM_A2_ARCHITECTURE: &str = "2";
const SEARCH_PAGE_SIZE: u32 = 25;
const MODELS_PAGE_SIZE: u32 = 50;

/// Gear types the amp slot can use: standalone amps and full amp+cab rigs.
const AMP_GEARS: &str = "amp_amp-cab";

/// Display-safe tone row for the editor. No download URLs, no credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToneCard {
    pub id: u64,
    pub title: String,
    pub creator: String,
    pub gear: String,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// A downloaded NAM A2 capture, ready to persist and load.
#[derive(Debug, Clone)]
pub struct DownloadedNam {
    pub tone_id: u64,
    pub title: String,
    pub creator: String,
    pub gear: String,
    pub size: String,
    pub file_stem: String,
    pub json: String,
}

#[derive(Debug, Deserialize)]
struct Paginated<T> {
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct ApiUser {
    #[serde(default)]
    username: String,
}

#[derive(Debug, Deserialize)]
struct ApiTone {
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    gear: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    images: serde_json::Value,
    #[serde(default)]
    user: Option<ApiUser>,
}

#[derive(Debug, Deserialize)]
struct ApiModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: String,
    #[serde(default)]
    architecture_version: serde_json::Value,
    #[serde(default)]
    model_url: String,
}

/// Whether this build can call TONE3000 (a baked or runtime API key is present).
pub fn configured() -> bool {
    api_key().is_some()
}

/// Baked by `build.rs` from the build environment or workspace `.env`.
const BAKED_API_KEY: Option<&str> = option_env!("FUTUREBOARD_TONE3000_API_KEY");

fn api_key() -> Option<String> {
    non_empty_env("FUTUREBOARD_TONE3000_API_KEY")
        .or_else(|| non_empty_env("TONE3000_API_KEY"))
        .or_else(|| BAKED_API_KEY.and_then(non_empty))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|value| non_empty(&value))
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn http_client(timeout_secs: u64) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .https_only(true)
        .user_agent("FutureboardStudio/Rodhareist")
        .build()
        .map_err(|error| format!("TONE3000 client failed to start: {error}"))
}

fn bearer_get(
    path_and_query: &str,
    timeout_secs: u64,
) -> Result<reqwest::blocking::Response, String> {
    let key = api_key().ok_or_else(|| {
        "TONE3000 API key is not configured (bake FUTUREBOARD_TONE3000_API_KEY at build, or set it at runtime)".to_string()
    })?;
    let url = if path_and_query.starts_with("http://") || path_and_query.starts_with("https://") {
        path_and_query.to_string()
    } else {
        format!("{API_BASE}{path_and_query}")
    };
    http_client(timeout_secs)?
        .get(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Accept", "application/json")
        .send()
        .map_err(|error| format!("TONE3000 request failed: {error}"))
}

fn require_ok(
    response: reqwest::blocking::Response,
    what: &str,
) -> Result<reqwest::blocking::Response, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().unwrap_or_default();
    let detail = body.trim();
    if detail.is_empty() {
        Err(format!("TONE3000 {what} failed ({status})"))
    } else {
        Err(format!(
            "TONE3000 {what} failed ({status}): {}",
            detail.chars().take(240).collect::<String>()
        ))
    }
}

/// Search public NAM A2 tones. Control / UI-background thread only.
pub fn search_tones(query: &str, page: u32) -> Result<(Vec<ToneCard>, u32), String> {
    let mut url = format!(
        "/api/v1/tones/search?format=nam&architecture={NAM_A2_ARCHITECTURE}&gears={AMP_GEARS}&page_size={SEARCH_PAGE_SIZE}&page={}",
        page.max(1)
    );
    let q = query.trim();
    if !q.is_empty() {
        url.push_str("&query=");
        url.push_str(&urlencoding(q));
        url.push_str("&sort=best-match");
    } else {
        url.push_str("&sort=trending");
    }
    let response = require_ok(bearer_get(&url, API_TIMEOUT_SECS)?, "search")?;
    let parsed: Paginated<ApiTone> = response
        .json()
        .map_err(|error| format!("TONE3000 search returned invalid JSON: {error}"))?;
    let cards = parsed.data.into_iter().map(tone_card).collect();
    Ok((cards, page.max(1)))
}

/// Fetch one tone and download its preferred NAM A2 model file.
pub fn download_a2_tone(
    tone_id: u64,
    preferred_size: Option<&str>,
) -> Result<DownloadedNam, String> {
    let tone = fetch_tone(tone_id)?;
    let models = list_a2_models(tone_id)?;
    let model = pick_a2_model(&models, preferred_size)
        .ok_or_else(|| format!("TONE3000 tone {tone_id} has no NAM A2 model to download"))?;
    if model.model_url.trim().is_empty() {
        return Err(format!(
            "TONE3000 tone {tone_id} did not include a download URL"
        ));
    }
    let response = require_ok(
        bearer_get(&model.model_url, DOWNLOAD_TIMEOUT_SECS)?,
        "download",
    )?;
    let json = response
        .text()
        .map_err(|error| format!("TONE3000 download could not be read: {error}"))?;
    if json.trim().is_empty() {
        return Err("TONE3000 download was empty".to_string());
    }
    let title = if tone.title.trim().is_empty() {
        if model.name.trim().is_empty() {
            format!("tone-{tone_id}")
        } else {
            model.name.clone()
        }
    } else {
        tone.title.clone()
    };
    Ok(DownloadedNam {
        tone_id,
        title: title.clone(),
        creator: tone
            .user
            .as_ref()
            .map(|user| user.username.clone())
            .unwrap_or_default(),
        gear: tone.gear,
        size: model.size.clone(),
        file_stem: file_stem_for(&title, &model.size),
        json,
    })
}

fn fetch_tone(tone_id: u64) -> Result<ApiTone, String> {
    let response = require_ok(
        bearer_get(
            &format!("/api/v1/tones/{tone_id}?architecture={NAM_A2_ARCHITECTURE}"),
            API_TIMEOUT_SECS,
        )?,
        "tone",
    )?;
    response
        .json()
        .map_err(|error| format!("TONE3000 tone {tone_id} returned invalid JSON: {error}"))
}

fn list_a2_models(tone_id: u64) -> Result<Vec<ApiModel>, String> {
    let url = format!(
        "/api/v1/models?tone_id={tone_id}&architecture={NAM_A2_ARCHITECTURE}&page_size={MODELS_PAGE_SIZE}&page=1"
    );
    let response = require_ok(bearer_get(&url, API_TIMEOUT_SECS)?, "models")?;
    let parsed: Paginated<ApiModel> = response
        .json()
        .map_err(|error| format!("TONE3000 models for {tone_id} returned invalid JSON: {error}"))?;
    Ok(parsed.data.into_iter().filter(is_a2_model).collect())
}

fn is_a2_model(model: &ApiModel) -> bool {
    architecture_is_a2(&model.architecture_version)
}

fn architecture_is_a2(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Number(n) => n.as_u64() == Some(2) || n.as_i64() == Some(2),
        serde_json::Value::String(s) => {
            let s = s.trim();
            s == "2" || s.eq_ignore_ascii_case("a2")
        }
        _ => false,
    }
}

fn pick_a2_model<'a>(models: &'a [ApiModel], preferred_size: Option<&str>) -> Option<&'a ApiModel> {
    if models.is_empty() {
        return None;
    }
    if let Some(wanted) = preferred_size.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(exact) = models
            .iter()
            .find(|model| model.size.eq_ignore_ascii_case(wanted))
        {
            return Some(exact);
        }
    }
    const ORDER: &[&str] = &["standard", "lite", "feather", "nano"];
    for size in ORDER {
        if let Some(model) = models
            .iter()
            .find(|model| model.size.eq_ignore_ascii_case(size))
        {
            return Some(model);
        }
    }
    models.first()
}

fn tone_card(tone: ApiTone) -> ToneCard {
    ToneCard {
        id: tone.id,
        title: if tone.title.trim().is_empty() {
            format!("Tone {}", tone.id)
        } else {
            tone.title
        },
        creator: tone
            .user
            .as_ref()
            .map(|user| user.username.clone())
            .unwrap_or_default(),
        gear: if tone.gear.trim().is_empty() {
            "amp".to_string()
        } else {
            tone.gear
        },
        format: if tone.format.trim().is_empty() {
            "nam".to_string()
        } else {
            tone.format
        },
        image: first_image_url(&tone.images),
    }
}

fn first_image_url(images: &serde_json::Value) -> Option<String> {
    match images {
        serde_json::Value::Array(items) => items.iter().find_map(|item| match item {
            serde_json::Value::String(url) if !url.trim().is_empty() => Some(url.clone()),
            serde_json::Value::Object(map) => map
                .get("url")
                .or_else(|| map.get("src"))
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_string),
            _ => None,
        }),
        serde_json::Value::String(url) if !url.trim().is_empty() => Some(url.clone()),
        _ => None,
    }
}

fn file_stem_for(title: &str, size: &str) -> String {
    let mut stem: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    if stem.is_empty() {
        stem = "TONE3000".to_string();
    }
    if !size.trim().is_empty() && !size.eq_ignore_ascii_case("standard") {
        stem.push(' ');
        stem.push_str(size.trim());
    }
    stem
}

fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Suggested `.nam` leaf name for the plugin NAMs folder.
pub fn nam_file_name(download: &DownloadedNam) -> String {
    format!("{}.nam", download.file_stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(size: &str, arch: serde_json::Value) -> ApiModel {
        ApiModel {
            name: format!("{size} model"),
            size: size.to_string(),
            architecture_version: arch,
            model_url: format!("https://www.tone3000.com/files/{size}.nam"),
        }
    }

    #[test]
    fn prefers_standard_a2_then_falls_back() {
        let models = vec![
            model("nano", serde_json::json!(2)),
            model("lite", serde_json::json!("2")),
            model("standard", serde_json::json!(2)),
        ];
        assert_eq!(pick_a2_model(&models, None).unwrap().size, "standard");
        assert_eq!(pick_a2_model(&models, Some("lite")).unwrap().size, "lite");
        let lite_only = vec![model("feather", serde_json::json!(2))];
        assert_eq!(pick_a2_model(&lite_only, None).unwrap().size, "feather");
        assert!(pick_a2_model(&[], None).is_none());
    }

    #[test]
    fn architecture_2_accepts_number_and_string() {
        assert!(architecture_is_a2(&serde_json::json!(2)));
        assert!(architecture_is_a2(&serde_json::json!("2")));
        assert!(architecture_is_a2(&serde_json::json!("a2")));
        assert!(!architecture_is_a2(&serde_json::json!(1)));
        assert!(!architecture_is_a2(&serde_json::json!("1")));
        assert!(!architecture_is_a2(&serde_json::Value::Null));
    }

    #[test]
    fn file_stem_is_a_safe_leaf() {
        let download = DownloadedNam {
            tone_id: 42,
            title: "Mesa ../Badlander!".into(),
            creator: "capturer".into(),
            gear: "amp".into(),
            size: "lite".into(),
            file_stem: file_stem_for("Mesa ../Badlander!", "lite"),
            json: "{}".into(),
        };
        assert_eq!(download.file_stem, "Mesa Badlander lite");
        assert_eq!(nam_file_name(&download), "Mesa Badlander lite.nam");
        assert!(!nam_file_name(&download).contains('/'));
        assert!(!nam_file_name(&download).contains(".."));
    }

    #[test]
    fn tone_card_fills_display_fields() {
        let card = tone_card(ApiTone {
            id: 7,
            title: "Twin Reverb".into(),
            gear: "amp".into(),
            format: "nam".into(),
            images: serde_json::json!(["https://cdn.example/t.png"]),
            user: Some(ApiUser {
                username: "jane".into(),
            }),
        });
        assert_eq!(card.id, 7);
        assert_eq!(card.title, "Twin Reverb");
        assert_eq!(card.creator, "jane");
        assert_eq!(card.image.as_deref(), Some("https://cdn.example/t.png"));
    }

    #[test]
    fn first_image_accepts_strings_or_url_objects() {
        assert_eq!(
            first_image_url(&serde_json::json!(["https://cdn.example/a.png"])).as_deref(),
            Some("https://cdn.example/a.png")
        );
        assert_eq!(
            first_image_url(&serde_json::json!([{"url": "https://cdn.example/b.png"}])).as_deref(),
            Some("https://cdn.example/b.png")
        );
        assert!(first_image_url(&serde_json::Value::Null).is_none());
    }

    #[test]
    fn empty_strings_are_not_api_keys() {
        assert!(non_empty("").is_none());
        assert!(non_empty("   ").is_none());
        assert_eq!(non_empty(" t3k_cs_x ").as_deref(), Some("t3k_cs_x"));
    }
}
