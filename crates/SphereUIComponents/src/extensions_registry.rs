//! Community extension registry client.
//!
//! Talks to the public registry on `futureboard.studio`. Every route used here
//! is unauthenticated, so browsing and installing needs no account.
//!
//! Control path only: all functions block on network or filesystem I/O and must
//! be called from GPUI's background executor, never from a render or audio path.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::Rgba;
use serde::Deserialize;

/// Override for local API work, e.g. `http://localhost:8080`. An override must
/// be HTTPS; plain HTTP is accepted only for a loopback host in a debug build.
const REGISTRY_BASE_URL_ENV: &str = "FUTUREBOARD_REGISTRY_URL";
const DEFAULT_REGISTRY_BASE_URL: &str = "https://futureboard.studio";
const REGISTRY_TIMEOUT_SECS: u64 = 10;
const REGISTRY_PAGE_SIZE: u32 = 60;

/// A theme document is a small JSON file. Anything larger is not one, and is
/// refused before it reaches the themes directory.
const MAX_THEME_DOCUMENT_BYTES: usize = 1 << 20;

/// Upper bound for one listing page (`REGISTRY_PAGE_SIZE` small rows).
const MAX_LISTING_BYTES: usize = 4 << 20;

/// Registry sections shown in the Extensions window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionKind {
    Plugins,
    Themes,
    AudioExtensions,
    Icons,
}

impl ExtensionKind {
    pub const ALL: [ExtensionKind; 4] = [
        ExtensionKind::Plugins,
        ExtensionKind::Themes,
        ExtensionKind::AudioExtensions,
        ExtensionKind::Icons,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExtensionKind::Plugins => "Plugins",
            ExtensionKind::Themes => "Themes",
            ExtensionKind::AudioExtensions => "Audio Extensions",
            ExtensionKind::Icons => "Icons",
        }
    }

    /// Whether the registry serves this section yet.
    ///
    /// Only themes are published today. The other sections are listed because
    /// they are part of the registry's shape, but the window must say they are
    /// empty rather than invent entries for them.
    pub fn is_available(self) -> bool {
        matches!(self, ExtensionKind::Themes)
    }

    /// Why an unavailable section is empty — shown in place of a list.
    pub fn unavailable_note(self) -> &'static str {
        match self {
            ExtensionKind::Plugins => "The registry does not publish plug-ins yet.",
            ExtensionKind::Themes => "",
            ExtensionKind::AudioExtensions => "The registry does not publish audio extensions yet.",
            ExtensionKind::Icons => "The registry does not publish icon packs yet.",
        }
    }
}

/// Swatch colors the registry precomputes for each theme, so a list row can be
/// drawn without downloading the whole token map.
#[derive(Debug, Clone, Default)]
pub struct RegistryPreview {
    pub background: Option<Rgba>,
    pub panel: Option<Rgba>,
    pub border: Option<Rgba>,
    pub text: Option<Rgba>,
    pub accent: Option<Rgba>,
    pub playhead: Option<Rgba>,
}

impl RegistryPreview {
    /// Swatches in draw order, skipping colors the theme did not define.
    pub fn swatches(&self) -> Vec<Rgba> {
        [
            self.background,
            self.panel,
            self.border,
            self.text,
            self.accent,
            self.playhead,
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

/// One registry entry as shown in the Extensions list.
#[derive(Debug, Clone)]
pub struct RegistryItem {
    pub slug: String,
    /// The extension's own stable id (`community.midnight-console`).
    pub extension_id: String,
    pub name: String,
    pub author: String,
    pub description: String,
    pub version: String,
    pub appearance: String,
    pub downloads: i64,
    pub preview: RegistryPreview,
}

// ── Wire types ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    items: Vec<ItemPayload>,
}

#[derive(Deserialize)]
struct ItemPayload {
    #[serde(default)]
    slug: String,
    #[serde(default, rename = "themeId")]
    theme_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    appearance: String,
    #[serde(default)]
    downloads: i64,
    #[serde(default)]
    preview: PreviewPayload,
}

#[derive(Deserialize, Default)]
struct PreviewPayload {
    #[serde(default)]
    background: Option<String>,
    #[serde(default)]
    panel: Option<String>,
    #[serde(default)]
    border: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    accent: Option<String>,
    #[serde(default)]
    playhead: Option<String>,
}

fn parse_swatch(value: Option<String>) -> Option<Rgba> {
    value
        .as_deref()
        .and_then(|hex| crate::color::parse_hex_color(hex).ok())
}

impl From<ItemPayload> for RegistryItem {
    fn from(payload: ItemPayload) -> Self {
        RegistryItem {
            slug: payload.slug,
            extension_id: payload.theme_id,
            name: payload.name,
            author: payload.author,
            description: payload.description,
            version: payload.version,
            appearance: payload.appearance,
            downloads: payload.downloads,
            preview: RegistryPreview {
                background: parse_swatch(payload.preview.background),
                panel: parse_swatch(payload.preview.panel),
                border: parse_swatch(payload.preview.border),
                text: parse_swatch(payload.preview.text),
                accent: parse_swatch(payload.preview.accent),
                playhead: parse_swatch(payload.preview.playhead),
            },
        }
    }
}

// ── Requests ────────────────────────────────────────────────────────────────

fn registry_base_url() -> Result<reqwest::Url, String> {
    match std::env::var(REGISTRY_BASE_URL_ENV) {
        Ok(value) if !value.trim().is_empty() => {
            parse_registry_base_url(&value, cfg!(debug_assertions))
        }
        _ => parse_registry_base_url(DEFAULT_REGISTRY_BASE_URL, false),
    }
}

/// Validates a registry base URL. HTTPS is always required, except that a
/// debug build may talk plain HTTP to a loopback host for local API work.
fn parse_registry_base_url(value: &str, allow_loopback_http: bool) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| format!("{REGISTRY_BASE_URL_ENV} is not a valid URL."))?;
    let host_is_loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    match url.scheme() {
        "https" if url.host().is_some() => {}
        "http" if allow_loopback_http && host_is_loopback => {}
        _ => {
            return Err(format!(
                "{REGISTRY_BASE_URL_ENV} must be an https:// URL (plain http is only accepted for localhost in a debug build)."
            ));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "{REGISTRY_BASE_URL_ENV} must not carry credentials."
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "{REGISTRY_BASE_URL_ENV} must not carry a query or fragment."
        ));
    }
    Ok(url)
}

/// The registry's theme routes live under `/theme-registry`, not `/themes` —
/// the marketing site owns `/themes` on the same origin. Each segment is
/// percent-encoded, so a remote slug cannot change the route.
fn theme_registry_url(
    base: &reqwest::Url,
    segments: &[&str],
    query: Option<&str>,
) -> Result<reqwest::Url, String> {
    let mut url = base.clone();
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| "The registry URL cannot hold a path.".to_string())?;
        path.pop_if_empty().push("theme-registry");
        for segment in segments {
            path.push(segment);
        }
    }
    url.set_query(query);
    Ok(url)
}

fn client(base: &reqwest::Url) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REGISTRY_TIMEOUT_SECS))
        // An HTTPS registry must not be redirected down to plain HTTP.
        .https_only(base.scheme() == "https")
        .build()
        .map_err(|error| format!("Could not create the registry client: {error}"))
}

/// Reads at most `max` bytes of a body. A larger body is refused as soon as
/// the limit is crossed, so an oversized response never lands in memory.
fn read_bounded(reader: impl Read, max: usize, what: &str) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    reader
        .take(max as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| format!("Could not read the {what}: {error}"))?;
    if body.len() > max {
        return Err(format!("The {what} is too large."));
    }
    Ok(body)
}

/// Rejects a response whose declared length is already over the limit, then
/// reads the body through [`read_bounded`] (the header can be absent or wrong).
fn read_response_bounded(
    response: reqwest::blocking::Response,
    max: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        return Err(format!("The {what} is too large."));
    }
    read_bounded(response, max, what)
}

/// Fetches one registry section.
///
/// Sections the registry does not serve return an empty list rather than an
/// error — the window renders their own explanatory note.
pub fn fetch_extensions(kind: ExtensionKind) -> Result<Vec<RegistryItem>, String> {
    if !kind.is_available() {
        return Ok(Vec::new());
    }

    let base = registry_base_url()?;
    let url = theme_registry_url(
        &base,
        &[],
        Some(&format!("sort=recent&limit={REGISTRY_PAGE_SIZE}")),
    )?;
    let response = client(&base)?
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|error| format!("Could not reach the extension registry: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("The registry returned HTTP {status}."));
    }

    let body = read_response_bounded(response, MAX_LISTING_BYTES, "registry listing")?;
    let payload = serde_json::from_slice::<ListResponse>(&body)
        .map_err(|error| format!("Could not read the registry listing: {error}"))?;

    Ok(payload.items.into_iter().map(RegistryItem::from).collect())
}

/// Where an installed theme lands: `<AppData>/Extensions/Themes/<slug>/theme.json`,
/// the same `**/theme.json` layout [`crate::theme`] discovers on startup.
pub fn installed_theme_path(slug: &str) -> PathBuf {
    crate::paths::FutureboardPaths::resolve()
        .themes
        .join(sanitize_slug(slug))
        .join("theme.json")
}

/// Whether a theme with this slug is already installed.
pub fn is_theme_installed(slug: &str) -> bool {
    installed_theme_path(slug).is_file()
}

/// The theme id of an already-installed slug, for
/// [`crate::theme::activate_theme_by_id`].
///
/// A slug is the registry's name for a download; a theme id is what the theme
/// calls itself, and only the second one selects it. Downloading returns the id,
/// but a theme installed in an earlier session has only its file — so this reads
/// the id back out of it.
pub fn installed_theme_id(slug: &str) -> Option<String> {
    let body = fs::read_to_string(installed_theme_path(slug)).ok()?;
    validate_theme_document(&body)
        .ok()
        .filter(|id| !id.is_empty())
}

/// Downloads one theme and installs it into the user themes directory.
///
/// The registry generates the document from its database on every request, so
/// what lands on disk is always current for the theme format. Returns the
/// installed path and the theme's own id, which is what
/// [`crate::theme::activate_theme_by_id`] takes.
pub fn download_theme(slug: &str) -> Result<(PathBuf, String), String> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err("That extension has no download id.".to_string());
    }

    let base = registry_base_url()?;
    let url = theme_registry_url(&base, &[slug, "download"], None)?;
    let response = client(&base)?
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|error| format!("Could not reach the extension registry: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("The registry returned HTTP {status}."));
    }

    let body = read_response_bounded(response, MAX_THEME_DOCUMENT_BYTES, "theme download")?;
    let body = String::from_utf8(body)
        .map_err(|_| "The registry did not return a theme document.".to_string())?;

    // Validate before writing: a misrouted request answers with an HTML page and
    // a 200, and an unreadable file in the themes directory would then fail on
    // every later scan instead of here, once.
    let theme_id = validate_theme_document(&body)?;

    let path = installed_theme_path(slug);
    let dir = path
        .parent()
        .ok_or_else(|| "Could not resolve the themes directory.".to_string())?;
    fs::create_dir_all(dir)
        .map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
    write_atomically(&path, body.as_bytes())
        .map_err(|error| format!("Could not write {}: {error}", path.display()))?;

    Ok((path, theme_id))
}

/// Confirms the payload is a theme document and returns its id.
fn validate_theme_document(body: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| "The registry did not return a theme document.".to_string())?;

    let tokens_ok = value
        .get("tokens")
        .and_then(|tokens| tokens.as_object())
        .is_some_and(|tokens| !tokens.is_empty());
    if !tokens_ok {
        return Err("That download has no theme tokens.".to_string());
    }

    Ok(value
        .get("id")
        .and_then(|id| id.as_str())
        .unwrap_or_default()
        .to_string())
}

/// Writes through a temporary file in the same directory, so an interrupted
/// download cannot leave a half-written `theme.json` for the next scan.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let temp = path.with_extension("json.part");
    fs::write(&temp, bytes)?;
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Keeps a registry slug to one safe directory name. The registry already emits
/// `[a-z0-9-]`, but this directory is user-visible and the value is remote.
fn sanitize_slug(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "extension".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_slug_refuses_path_traversal() {
        assert_eq!(sanitize_slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_slug("midnight-console"), "midnight-console");
        assert_eq!(sanitize_slug("..."), "extension");
        assert_eq!(sanitize_slug(""), "extension");
    }

    #[test]
    fn validate_theme_document_requires_tokens() {
        let ok = r##"{"id":"community.x","tokens":{"surface":{"base":"#101014"}}}"##;
        assert_eq!(validate_theme_document(ok).unwrap(), "community.x");

        // A misrouted request answers with a page, not a theme.
        assert!(validate_theme_document("<!doctype html><html></html>").is_err());
        assert!(validate_theme_document(r#"{"id":"community.x"}"#).is_err());
        assert!(validate_theme_document(r#"{"tokens":{}}"#).is_err());
    }

    #[test]
    fn bounded_read_stops_at_the_limit() {
        // An endless body: reading it whole would never finish.
        let endless = std::io::repeat(b'x');
        assert!(read_bounded(endless, MAX_THEME_DOCUMENT_BYTES, "theme download").is_err());

        let exact = [b'a'; 16];
        assert_eq!(read_bounded(&exact[..], 16, "x").unwrap().len(), 16);
        assert!(read_bounded(&[b'a'; 17][..], 16, "x").is_err());
    }

    #[test]
    fn registry_override_requires_https() {
        assert!(parse_registry_base_url("https://registry.example", false).is_ok());
        assert!(parse_registry_base_url("http://registry.example", false).is_err());
        assert!(parse_registry_base_url("http://registry.example", true).is_err());
        assert!(parse_registry_base_url("http://localhost:8080", false).is_err());
        assert!(parse_registry_base_url("ftp://registry.example", false).is_err());
        assert!(parse_registry_base_url("not a url", false).is_err());
        assert!(parse_registry_base_url("https://user:pw@registry.example", false).is_err());
        // Local API work in a debug build only.
        assert!(parse_registry_base_url("http://localhost:8080", true).is_ok());
        assert!(parse_registry_base_url("http://127.0.0.1:8080", true).is_ok());
        assert!(parse_registry_base_url("http://[::1]:8080", true).is_ok());
        assert!(parse_registry_base_url(DEFAULT_REGISTRY_BASE_URL, false).is_ok());
    }

    #[test]
    fn theme_registry_url_encodes_the_slug() {
        let base = parse_registry_base_url("https://futureboard.studio/", false).unwrap();
        let url = theme_registry_url(&base, &["midnight", "download"], None).unwrap();
        assert_eq!(
            url.as_str(),
            "https://futureboard.studio/theme-registry/midnight/download"
        );

        let hostile = theme_registry_url(&base, &["../../admin?x=1#y", "download"], None).unwrap();
        assert_eq!(hostile.host_str(), Some("futureboard.studio"));
        assert!(hostile.path().starts_with("/theme-registry/"));
        assert!(hostile.path().ends_with("/download"));
        assert!(hostile.query().is_none());
        assert!(hostile.fragment().is_none());
        assert_eq!(hostile.path_segments().unwrap().count(), 3);

        let listing = theme_registry_url(&base, &[], Some("sort=recent&limit=60")).unwrap();
        assert_eq!(
            listing.as_str(),
            "https://futureboard.studio/theme-registry?sort=recent&limit=60"
        );
    }

    #[test]
    fn unavailable_sections_return_no_items() {
        // No network: an unavailable section must not issue a request at all.
        assert!(fetch_extensions(ExtensionKind::Icons).unwrap().is_empty());
        assert!(fetch_extensions(ExtensionKind::Plugins).unwrap().is_empty());
    }
}
