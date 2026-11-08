//! GitHub Releases based application update discovery and staging.
//!
//! Network and filesystem work in this module is blocking by design. Callers
//! must run it on GPUI's background executor, never on the UI or audio thread.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sphere_ui_components::settings::UpdateChannel;
use sphere_ui_components::update_service::{
    DownloadProgressFn, InstallOutcome, UpdateCandidate, UpdateProvider,
};

/// The repository was renamed to `futureboard-studio`. The old path still
/// resolves only because GitHub answers a rename with a 301, which every
/// shipped build follows silently — a redirect that disappears the moment the
/// old name is claimed by anything else. Ask for the real repository.
const RELEASES_API: &str = "https://api.github.com/repos/futureboard/futureboard-studio/releases";
const USER_AGENT: &str = "Futureboard-Studio-Updater";

/// The project's Ed25519 update-signing public key, as 64 hex characters,
/// baked in at build time from `FUTUREBOARD_UPDATE_PUBLIC_KEY`.
///
/// A matching SHA-256 from GitHub only proves the bytes arrived intact: anyone
/// able to publish a release asset can publish its hash too. Authenticity comes
/// from this key, whose private half never touches a build. A build without it
/// cannot verify anything, so it refuses every download rather than trusting
/// the release host.
const UPDATE_PUBLIC_KEY: Option<&str> = option_env!("FUTUREBOARD_UPDATE_PUBLIC_KEY");

/// Each installer `<asset>` is published with a detached signature asset named
/// `<asset>.sig`.
const SIGNATURE_SUFFIX: &str = ".sig";

/// Domain separation for the signed message, so an update signature can never
/// be replayed as any other Ed25519 signature made with the same key (and vice
/// versa). Bump the version only together with the release signer.
const SIGNATURE_CONTEXT: &[u8] = b"futureboard-studio-update-v1\n";

/// A signature asset is 64 raw bytes or 128 hex characters; anything this large
/// is not one, so it is rejected without reading further.
const MAX_SIGNATURE_BYTES: u64 = 4096;

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub version: Version,
    pub channel: UpdateChannel,
    asset: GithubAsset,
    /// The `<asset>.sig` published beside the installer. A release without one
    /// is never offered.
    signature: GithubAsset,
}

/// `Ok(None)` when GitHub answered 404.
///
/// That is the ordinary reply for a channel with nothing published — the
/// nightly channel asks for `releases/tags/nightly` by name, and no such
/// release exists until the first nightly of a cycle is uploaded. The check
/// runs automatically after launch, so treating it as a failure would show
/// "GitHub API request failed" to a user who simply has no update waiting.
/// Every other transport or status failure stays an error.
fn get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<Option<T>, String> {
    let response = match ureq::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT)
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(error) => return Err(format!("GitHub API request failed: {error}")),
    };
    serde_json::from_reader(response.into_body().into_reader())
        .map(Some)
        .map_err(|error| format!("GitHub API returned invalid JSON: {error}"))
}

/// Installers are named `Futureboard.Studio-<version>-<platform>.<ext>`, with
/// `<version>` written straight from `version.json` by the release workflow —
/// which also verifies these exact shapes before it uploads. That makes the
/// filename a machine-written copy of the version, which a release title is not.
const ASSET_NAME_PREFIX: &str = "futureboard.studio-";

/// The platform tails that follow the version, lowercased for matching.
///
/// Spelling them out is what makes the split reliable: semver's prerelease
/// grammar accepts hyphens inside an identifier, so `2026.8.9-beta1-windows`
/// parses happily as a version with the prerelease `beta1-windows`. The tail
/// cannot be inferred by scanning — it has to be known.
const ASSET_PLATFORM_SUFFIXES: &[&str] = &[
    "-windows-x86_64-setup.exe",
    "-macos-universal.dmg",
    "-macos-arm64.dmg",
    "-macos-x86_64.dmg",
    "-x86_64.appimage",
    // Releases predating architecture-tagged DMG names.
    "-macos.dmg",
];

fn version_from_asset_name(name: &str) -> Option<Version> {
    let lower = name.to_ascii_lowercase();
    let rest = lower.strip_prefix(ASSET_NAME_PREFIX)?;
    let version = ASSET_PLATFORM_SUFFIXES
        .iter()
        .find_map(|suffix| rest.strip_suffix(suffix))?;
    Version::parse(version).ok()
}

/// The version a release offers, or `None` if it cannot be established — such a
/// release is skipped entirely.
///
/// A version tag answers this outright. A *moving* tag cannot: `nightly` is
/// republished in place every day, and 2026.8.9-beta1 went out on `beta`, where
/// `Version::parse("beta")` fails and the whole release became invisible to the
/// updater.
///
/// The fallback reads the asset this machine would actually install, so the
/// version offered is the version that arrives — and a release carrying a
/// leftover asset from an older build cannot advertise that older version.
///
/// Asset names are preferred over the release name because a title is prose: for
/// this very release it reads "Futureboard Studio 2026.8.9 Beta 1", whose only
/// parseable token is `2026.8.9` — which *outranks* the `2026.8.9-beta1` inside
/// the installer, and would offer every user a permanent update to the build
/// they are already running.
fn release_version(release: &GithubRelease) -> Option<Version> {
    let tag = release.tag_name.trim().trim_start_matches('v');
    if let Ok(version) = Version::parse(tag) {
        return Some(version);
    }

    platform_asset(release)
        .and_then(|asset| version_from_asset_name(&asset.name))
        .or_else(|| version_from_release_name(release))
}

/// Last resort for a moving tag whose assets predate the naming convention.
fn version_from_release_name(release: &GithubRelease) -> Option<Version> {
    release
        .name
        .as_deref()?
        .split_whitespace()
        .find_map(|word| {
            let candidate = word
                .trim_matches(|character: char| {
                    !character.is_ascii_alphanumeric() && character != '.' && character != '-'
                })
                .trim_start_matches('v');
            Version::parse(candidate).ok()
        })
}

fn release_matches_channel(release: &GithubRelease, channel: UpdateChannel) -> bool {
    if release.draft {
        return false;
    }
    let Some(version) = release_version(release) else {
        return false;
    };
    match channel {
        UpdateChannel::Nightly => release.tag_name == "nightly",
        // Beta users also receive stable releases. Trust the semantic version
        // here because older repository releases were occasionally published
        // with an incorrect GitHub `prerelease` flag.
        UpdateChannel::Beta => release.tag_name != "nightly",
        UpdateChannel::Stable => {
            release.tag_name != "nightly" && !release.prerelease && version.pre.is_empty()
        }
    }
}

/// How well an asset name fits this build, lower being better. `None` means the
/// asset is not installable here at all.
///
/// Only macOS has more than one candidate: releases carry a universal DMG plus
/// a per-architecture one. A plain `find` would hand whichever came first —
/// possibly the Intel image to an Apple Silicon machine — so the match is
/// ranked instead of taken greedily.
fn asset_rank(name: &str) -> Option<u8> {
    #[cfg(target_os = "windows")]
    {
        return (name.ends_with(".exe") && name.contains("setup")).then_some(0);
    }
    #[cfg(target_os = "linux")]
    {
        return (name.ends_with(".appimage") && name.contains("x86_64")).then_some(0);
    }
    #[cfg(target_os = "macos")]
    {
        if !name.ends_with(".dmg") || !name.contains("macos") {
            return None;
        }
        #[cfg(target_arch = "aarch64")]
        let (native, foreign) = ("macos-arm64", "macos-x86_64");
        #[cfg(target_arch = "x86_64")]
        let (native, foreign) = ("macos-x86_64", "macos-arm64");
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        let (native, foreign) = ("macos-universal", "\0");

        if name.contains(native) {
            return Some(0);
        }
        if name.contains("macos-universal") {
            return Some(1);
        }
        if name.contains(foreign) {
            // Running the other architecture's image would mean installing a
            // Rosetta-only (or unrunnable) build over a native one.
            return None;
        }
        // Releases published before the DMG name carried an architecture.
        return Some(2);
    }
    #[allow(unreachable_code)]
    {
        let _ = name;
        None
    }
}

fn platform_asset(release: &GithubRelease) -> Option<GithubAsset> {
    release
        .assets
        .iter()
        .filter_map(|asset| asset_rank(&asset.name.to_ascii_lowercase()).map(|rank| (rank, asset)))
        // Ties keep the first asset, matching the previous `find` behaviour.
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, asset)| asset.clone())
}

/// The detached signature published for `asset`: exactly `<asset name>.sig`.
fn signature_asset(release: &GithubRelease, asset: &GithubAsset) -> Option<GithubAsset> {
    let expected = format!("{}{SIGNATURE_SUFFIX}", asset.name);
    release
        .assets
        .iter()
        .find(|candidate| candidate.name == expected)
        .cloned()
}

/// The exact bytes the release signer signs for one installer:
/// `"futureboard-studio-update-v1\n" || <asset name> || "\n" || SHA-256(installer)`
/// with the digest as 32 raw bytes.
///
/// Binding the asset name (which carries the version and platform) means a
/// genuine signature for one installer cannot vouch for a different one — an
/// older build or another platform's — published under this name.
fn signed_message(asset_name: &str, sha256: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(SIGNATURE_CONTEXT.len() + asset_name.len() + 1 + 32);
    message.extend_from_slice(SIGNATURE_CONTEXT);
    message.extend_from_slice(asset_name.as_bytes());
    message.push(b'\n');
    message.extend_from_slice(sha256);
    message
}

fn decode_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.as_bytes();
    if text.len() != N * 2 {
        return None;
    }
    let nibble = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
    let mut out = [0_u8; N];
    for (slot, pair) in out.iter_mut().zip(text.chunks_exact(2)) {
        *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

fn parse_public_key(hex: &str) -> Result<VerifyingKey, String> {
    let bytes = decode_hex::<32>(hex.trim())
        .ok_or_else(|| "the update signing key baked into this build is malformed".to_string())?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|_| "the update signing key baked into this build is invalid".to_string())
}

/// Accepts the signature as 64 raw bytes (what `openssl pkeyutl -sign` writes)
/// or as 128 hex characters with optional surrounding whitespace.
fn parse_signature(raw: &[u8]) -> Result<Signature, String> {
    let bytes = if raw.len() == Signature::BYTE_SIZE {
        let mut bytes = [0_u8; Signature::BYTE_SIZE];
        bytes.copy_from_slice(raw);
        Some(bytes)
    } else {
        std::str::from_utf8(raw)
            .ok()
            .and_then(|text| decode_hex::<{ Signature::BYTE_SIZE }>(text.trim()))
    };
    bytes
        .map(|bytes| Signature::from_bytes(&bytes))
        .ok_or_else(|| "the update signature is malformed".to_string())
}

const NO_KEY_ERROR: &str = "updates are unavailable in this build: it was built without an update \
     signing key (FUTUREBOARD_UPDATE_PUBLIC_KEY), so no installer could be verified";

/// The baked-in key, or why this build cannot verify (and so cannot offer or
/// download) any update.
fn update_verifying_key(public_key: Option<&str>) -> Result<VerifyingKey, String> {
    let public_key = public_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| NO_KEY_ERROR.to_string())?;
    parse_public_key(public_key)
}

/// Fail closed: no key, a malformed key, a malformed signature or a signature
/// that does not verify all reject the update.
fn verify_update_signature(
    public_key: Option<&str>,
    asset_name: &str,
    sha256: &[u8; 32],
    signature: &[u8],
) -> Result<(), String> {
    let key = update_verifying_key(public_key)?;
    let signature = parse_signature(signature)?;
    key.verify_strict(&signed_message(asset_name, sha256), &signature)
        .map_err(|_| "the update signature is not valid for this installer".to_string())
}

/// The signature binds the asset name, and the name carries the version. The
/// version offered may come from the release tag instead, so the two must
/// agree: otherwise a genuinely signed older installer could be re-published
/// under a newer tag and install as a silent downgrade.
fn asset_carries_offered_version(update: &AvailableUpdate) -> bool {
    version_from_asset_name(&update.asset.name).as_ref() == Some(&update.version)
}

/// Fetch the small detached signature asset, bounded so a hostile response
/// cannot make the updater buffer an arbitrary amount.
fn download_signature(asset: &GithubAsset) -> Result<Vec<u8>, String> {
    if asset.size > MAX_SIGNATURE_BYTES {
        return Err("the update signature asset is too large".to_string());
    }
    let response = ureq::get(&asset.browser_download_url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| format!("update signature download failed: {error}"))?;
    let mut signature = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_SIGNATURE_BYTES + 1)
        .read_to_end(&mut signature)
        .map_err(|error| format!("failed to download the update signature: {error}"))?;
    if signature.len() as u64 > MAX_SIGNATURE_BYTES {
        return Err("the update signature asset is too large".to_string());
    }
    Ok(signature)
}

fn choose_release(
    releases: Vec<GithubRelease>,
    channel: UpdateChannel,
    current: &Version,
) -> Option<AvailableUpdate> {
    releases
        .into_iter()
        .filter(|release| release_matches_channel(release, channel))
        .filter_map(|release| {
            let version = release_version(&release)?;
            let asset = platform_asset(&release)?;
            // Never offer what `download_update` would refuse: an installer
            // with no published signature, or one whose (signed) name does
            // not carry the version being offered.
            let signature = signature_asset(&release, &asset)?;
            if version_from_asset_name(&asset.name).as_ref() != Some(&version) {
                return None;
            }
            let available = match channel {
                // A user explicitly opting into nightly may move from the same
                // stable base to its prerelease build.
                UpdateChannel::Nightly => version != *current,
                UpdateChannel::Beta | UpdateChannel::Stable => version > *current,
            };
            available.then(|| AvailableUpdate {
                version,
                channel,
                asset,
                signature,
            })
        })
        .max_by(|left, right| left.version.cmp(&right.version))
}

pub fn check_for_update(
    channel: UpdateChannel,
    current_version: &str,
) -> Result<Option<AvailableUpdate>, String> {
    check_for_update_with_key(UPDATE_PUBLIC_KEY, channel, current_version)
}

fn check_for_update_with_key(
    public_key: Option<&str>,
    channel: UpdateChannel,
    current_version: &str,
) -> Result<Option<AvailableUpdate>, String> {
    // A build that cannot verify an installer must not offer one. This is an
    // error, not "up to date": the user is told why no update can be offered.
    update_verifying_key(public_key)?;
    let current = Version::parse(current_version)
        .map_err(|error| format!("invalid installed version {current_version:?}: {error}"))?;
    let url = match channel {
        UpdateChannel::Nightly => format!("{RELEASES_API}/tags/nightly"),
        UpdateChannel::Beta | UpdateChannel::Stable => format!("{RELEASES_API}?per_page=30"),
    };
    let releases = if channel == UpdateChannel::Nightly {
        // A missing `nightly` release means no nightly has been published, not
        // that the lookup failed.
        match get_json::<GithubRelease>(&url)? {
            Some(release) => vec![release],
            None => return Ok(None),
        }
    } else {
        get_json::<Vec<GithubRelease>>(&url)?.unwrap_or_default()
    };
    Ok(choose_release(releases, channel, &current))
}

pub fn download_update(
    update: &AvailableUpdate,
    cache_root: &Path,
    progress: &dyn Fn(u64, u64),
) -> Result<PathBuf, String> {
    let file_name = Path::new(&update.asset.name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "release asset has an invalid filename".to_string())?;
    let update_dir = cache_root.join("Updates").join(update.version.to_string());
    fs::create_dir_all(&update_dir)
        .map_err(|error| format!("failed to create update cache: {error}"))?;
    let destination = update_dir.join(file_name);
    let partial = destination.with_extension("part");

    // Settle everything that would make the installer unverifiable before
    // spending the bandwidth on it.
    update_verifying_key(UPDATE_PUBLIC_KEY)?;
    if !asset_carries_offered_version(update) {
        return Err(format!(
            "the installer name {} does not carry the offered version {}",
            update.asset.name, update.version
        ));
    }
    let signature = download_signature(&update.signature)?;

    let response = ureq::get(&update.asset.browser_download_url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| format!("update download failed: {error}"))?;
    let mut reader = response.into_body().into_reader();
    let mut writer = BufWriter::new(
        File::create(&partial).map_err(|error| format!("failed to create update file: {error}"))?,
    );
    let mut hasher = Sha256::new();
    let mut downloaded = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => {
                drop(writer);
                let _ = fs::remove_file(&partial);
                return Err(format!("failed to download update: {error}"));
            }
        };
        if let Err(error) = writer.write_all(&buffer[..count]) {
            drop(writer);
            let _ = fs::remove_file(&partial);
            return Err(format!("failed to save update: {error}"));
        }
        hasher.update(&buffer[..count]);
        downloaded = downloaded.saturating_add(count as u64);
        progress(downloaded, update.asset.size);
    }
    if let Err(error) = writer.flush() {
        drop(writer);
        let _ = fs::remove_file(&partial);
        return Err(format!("failed to flush update: {error}"));
    }
    drop(writer);
    if downloaded != update.asset.size {
        let _ = fs::remove_file(&partial);
        return Err(format!(
            "download size mismatch: expected {} bytes, received {downloaded}",
            update.asset.size
        ));
    }
    let digest = hasher.finalize();
    let actual_digest = format!("sha256:{digest:x}");
    let digest: [u8; 32] = digest.into();
    if update
        .asset
        .digest
        .as_deref()
        .is_none_or(|expected| !expected.eq_ignore_ascii_case(&actual_digest))
    {
        let _ = fs::remove_file(&partial);
        return Err("GitHub did not provide a matching SHA-256 digest for the update".to_string());
    }
    // The digest above is integrity only. Authenticity is the project's
    // signature over that digest; an installer that fails it never reaches the
    // final path that `install_update` runs.
    if let Err(error) =
        verify_update_signature(UPDATE_PUBLIC_KEY, &update.asset.name, &digest, &signature)
    {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    if destination.exists() {
        fs::remove_file(&destination)
            .map_err(|error| format!("failed to replace cached update: {error}"))?;
    }
    fs::rename(&partial, &destination)
        .map_err(|error| format!("failed to finalize update: {error}"))?;
    Ok(destination)
}

/// Hand the staged download to the platform installer.
///
/// Every platform path either replaces this installation in place — which needs
/// the process to exit first — or hands the file to the platform and lets the
/// user drive it.
pub fn install_update(staged: &Path, cache_root: &Path) -> Result<InstallOutcome, String> {
    #[cfg(target_os = "windows")]
    return install_windows(staged, cache_root);
    #[cfg(target_os = "macos")]
    return install_macos(staged, cache_root);
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    return install_linux(staged, cache_root);
}

/// Windows: the release asset is the Inno Setup installer. Run it with Inno's
/// command-line switches so the update applies to the *current* installation
/// (same directory, same scope) instead of opening a fresh wizard.
///
/// Switch reference (Inno Setup 6):
///   `/SILENT`             progress window only, no wizard pages
///   `/SP-`                skip the "This will install..." prompt
///   `/SUPPRESSMSGBOXES`   accept the default answer for setup message boxes
///   `/NORESTART`          never reboot the machine on our behalf
///   `/CLOSEAPPLICATIONS`  let Restart Manager close remaining app processes
///   `/NORESTARTAPPLICATIONS` leave the relaunch to the `[Run]` entry below,
///                        so Restart Manager cannot open a second copy
///   `/RELAUNCH`          our own switch, not Inno's: `installer.iss` reopens
///                        the app when it is present on a silent run
///   `/DIR`, `/ALLUSERS` / `/CURRENTUSER`  keep the existing install location
///
/// The relaunch matters because a silent update is started *by* the running
/// app, which then quits so setup can replace its files. Inno's `[Run]`
/// checkbox carries `skipifsilent`, so without `/RELAUNCH` an update ends with
/// the application closed and nothing to bring it back.
///
/// `installer.iss` declares `PrivilegesRequired=lowest` with
/// `PrivilegesRequiredOverridesAllowed=dialog commandline`, so `/ALLUSERS`
/// is the supported way to update a machine-wide install (Windows shows the
/// elevation prompt).
#[cfg(target_os = "windows")]
fn install_windows(staged: &Path, cache_root: &Path) -> Result<InstallOutcome, String> {
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let log_path = cache_root.join("Updates").join("install.log");

    let mut command = Command::new(staged);
    command
        .arg("/SILENT")
        .arg("/SP-")
        .arg("/SUPPRESSMSGBOXES")
        .arg("/NORESTART")
        .arg("/CLOSEAPPLICATIONS")
        .arg("/NORESTARTAPPLICATIONS")
        .arg("/RELAUNCH")
        .arg(format!("/LOG={}", log_path.display()));
    if let Some(dir) = install_dir.as_deref() {
        command.arg(format!("/DIR={}", dir.display()));
        command.arg(if is_machine_wide(dir) {
            "/ALLUSERS"
        } else {
            "/CURRENTUSER"
        });
    }
    command
        .spawn()
        .map_err(|error| format!("failed to start the installer: {error}"))?;
    Ok(InstallOutcome::QuitRequired)
}

/// True when the install directory sits under a Program Files root, which is
/// what an all-users Inno install produces.
#[cfg(target_os = "windows")]
fn is_machine_wide(install_dir: &Path) -> bool {
    ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"]
        .iter()
        .filter_map(std::env::var_os)
        .any(|root| install_dir.starts_with(PathBuf::from(root)))
}

/// macOS: the asset is a DMG. Mount it, copy the bundle out, unmount, then let
/// a detached shell script swap the running bundle once this process exits.
///
/// The swap no longer clears `com.apple.quarantine`. That does not by itself
/// bring in a Gatekeeper assessment: a DMG fetched by `ureq` never receives a
/// quarantine attribute, so the bundle copied out of it has none either. It only
/// means the updater never actively strips one. Authenticity is established
/// earlier, by the project signature `download_update` verifies before the DMG
/// reaches this function.
#[cfg(target_os = "macos")]
fn install_macos(staged: &Path, cache_root: &Path) -> Result<InstallOutcome, String> {
    let Some(target) = current_macos_bundle() else {
        // Not running from a bundle (cargo run, CI). Reveal the DMG instead of
        // guessing an install location.
        Command::new("open")
            .arg(staged)
            .spawn()
            .map_err(|error| format!("failed to open the disk image: {error}"))?;
        return Ok(InstallOutcome::Handoff);
    };

    let work_dir = cache_root.join("Updates").join("macos");
    let _ = fs::remove_dir_all(&work_dir);
    fs::create_dir_all(&work_dir)
        .map_err(|error| format!("failed to create the update workspace: {error}"))?;
    let mount_point = work_dir.join("mount");
    fs::create_dir_all(&mount_point)
        .map_err(|error| format!("failed to create the disk image mount point: {error}"))?;

    run_tool(
        "hdiutil",
        &[
            "attach".as_ref(),
            staged.as_os_str(),
            "-nobrowse".as_ref(),
            "-quiet".as_ref(),
            "-mountpoint".as_ref(),
            mount_point.as_os_str(),
        ],
    )?;

    let mounted_bundle = fs::read_dir(&mount_point)
        .map_err(|error| format!("failed to read the disk image: {error}"))?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "app"));
    let mounted_bundle = match mounted_bundle {
        Some(bundle) => bundle,
        None => {
            let _ = run_tool(
                "hdiutil",
                &[
                    "detach".as_ref(),
                    mount_point.as_os_str(),
                    "-quiet".as_ref(),
                ],
            );
            return Err("the disk image does not contain an application bundle".to_string());
        }
    };

    let bundle_name = mounted_bundle
        .file_name()
        .ok_or_else(|| "the disk image bundle has no name".to_string())?;
    let staged_bundle = work_dir.join(bundle_name);
    let copy_result = run_tool(
        "ditto",
        &[mounted_bundle.as_os_str(), staged_bundle.as_os_str()],
    );
    let _ = run_tool(
        "hdiutil",
        &[
            "detach".as_ref(),
            mount_point.as_os_str(),
            "-quiet".as_ref(),
        ],
    );
    copy_result?;

    let script_path = work_dir.join("swap-bundle.sh");
    let script = format!(
        "#!/bin/sh\n\
         while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n\
         /bin/rm -rf \"{target}\"\n\
         /usr/bin/ditto \"{staged}\" \"{target}\" || exit 1\n\
         /bin/rm -rf \"{staged}\"\n\
         /usr/bin/open \"{target}\"\n",
        pid = std::process::id(),
        target = target.display(),
        staged = staged_bundle.display(),
    );
    fs::write(&script_path, script)
        .map_err(|error| format!("failed to write the update script: {error}"))?;
    spawn_detached_script(&script_path)?;
    Ok(InstallOutcome::QuitRequired)
}

/// `/Applications/Futureboard Studio.app` for
/// `/Applications/Futureboard Studio.app/Contents/MacOS/FutureboardNative`.
#[cfg(target_os = "macos")]
fn current_macos_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .map(Path::to_path_buf)
}

/// Linux: the asset is an AppImage. When Studio is itself running from an
/// AppImage, replace that file and relaunch it after this process exits;
/// otherwise just run the downloaded image.
#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn install_linux(staged: &Path, cache_root: &Path) -> Result<InstallOutcome, String> {
    make_executable(staged)?;

    let Some(current) = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    else {
        Command::new(staged)
            .spawn()
            .map_err(|error| format!("failed to start the AppImage: {error}"))?;
        return Ok(InstallOutcome::Handoff);
    };

    // Replace via a sibling temp file + rename so the swap is atomic and stays
    // on the same filesystem as the running image.
    let replacement = current.with_extension("AppImage.update");
    fs::copy(staged, &replacement)
        .map_err(|error| format!("failed to stage the AppImage update: {error}"))?;
    make_executable(&replacement)?;
    fs::rename(&replacement, &current).map_err(|error| {
        let _ = fs::remove_file(&replacement);
        format!("failed to install the AppImage update: {error}")
    })?;

    let work_dir = cache_root.join("Updates");
    fs::create_dir_all(&work_dir)
        .map_err(|error| format!("failed to create the update workspace: {error}"))?;
    let script_path = work_dir.join("relaunch-appimage.sh");
    let script = format!(
        "#!/bin/sh\n\
         while kill -0 {pid} 2>/dev/null; do sleep 0.2; done\n\
         exec \"{target}\"\n",
        pid = std::process::id(),
        target = current.display(),
    );
    fs::write(&script_path, script)
        .map_err(|error| format!("failed to write the relaunch script: {error}"))?;
    spawn_detached_script(&script_path)?;
    Ok(InstallOutcome::QuitRequired)
}

#[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("failed to inspect the AppImage: {error}"))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("failed to make the AppImage executable: {error}"))
}

#[cfg(not(target_os = "windows"))]
fn run_tool(program: &str, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("failed to run {program}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    Err(if detail.is_empty() {
        format!("{program} failed with status {}", output.status)
    } else {
        format!("{program} failed: {detail}")
    })
}

/// Run the swap/relaunch helper in its own session so it outlives this process.
#[cfg(not(target_os = "windows"))]
fn spawn_detached_script(script: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(script)
        .map_err(|error| format!("failed to inspect the update script: {error}"))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(script, permissions)
        .map_err(|error| format!("failed to prepare the update script: {error}"))?;

    // The child is reparented to init once this process exits, so no extra
    // detach step is needed; only the standard streams must be released.
    Command::new("/bin/sh")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to start the update script: {error}"))
}

/// Update provider registered with `sphere_ui_components` at boot so the
/// Software Update dialog can drive this module.
pub struct GithubUpdateProvider;

impl UpdateProvider for GithubUpdateProvider {
    fn current_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    fn check(&self, channel: UpdateChannel) -> Result<Option<UpdateCandidate>, String> {
        Ok(check_for_update(channel, env!("CARGO_PKG_VERSION"))?.map(candidate_from))
    }

    fn download(
        &self,
        candidate: &UpdateCandidate,
        progress: DownloadProgressFn<'_>,
    ) -> Result<PathBuf, String> {
        let update = candidate
            .payload
            .downcast_ref::<AvailableUpdate>()
            .ok_or_else(|| "update candidate came from a different provider".to_string())?;
        download_update(update, &cache_root(), &|received, total| {
            progress(received, total)
        })
    }

    fn install(&self, staged: &Path) -> Result<InstallOutcome, String> {
        install_update(staged, &cache_root())
    }
}

/// Root of the update cache. Shared with the Professional transport, which
/// stages its own downloads in the same place before reusing
/// [`install_update`].
pub(crate) fn cache_root() -> PathBuf {
    sphere_ui_components::paths::FutureboardPaths::resolve().app_cache
}

pub fn candidate_from(update: AvailableUpdate) -> UpdateCandidate {
    UpdateCandidate {
        version: update.version.to_string(),
        channel: update.channel,
        asset_name: update.asset.name.clone(),
        asset_size: update.asset.size,
        release_url: None,
        payload: Arc::new(update),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, name: &str, prerelease: bool) -> GithubRelease {
        let mut release = versioned_release(tag, name, tag.trim_start_matches('v'));
        release.prerelease = prerelease;
        release
    }

    /// The detached signature asset the release workflow publishes beside
    /// `installer`.
    fn signature_for(installer: &str) -> GithubAsset {
        GithubAsset {
            name: format!("{installer}.sig"),
            browser_download_url: "https://example.test/download.sig".to_string(),
            size: 64,
            digest: None,
        }
    }

    /// A release now carries a universal DMG *and* both single-architecture
    /// ones. The native image must win, the universal image must be the
    /// fallback, and the other architecture's image must never be chosen.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_prefers_native_then_universal_and_never_the_other_arch() {
        let dmg = |name: &str| GithubAsset {
            name: name.to_string(),
            browser_download_url: "https://example.test/download".to_string(),
            size: 1,
            digest: None,
        };
        let with = |assets: Vec<GithubAsset>| GithubRelease {
            tag_name: "2026.8.8".to_string(),
            name: Some("Release".to_string()),
            draft: false,
            prerelease: false,
            assets,
        };

        let native = if cfg!(target_arch = "aarch64") {
            "Futureboard.Studio-2026.8.8-macos-arm64.dmg"
        } else {
            "Futureboard.Studio-2026.8.8-macos-x86_64.dmg"
        };
        let foreign = if cfg!(target_arch = "aarch64") {
            "Futureboard.Studio-2026.8.8-macos-x86_64.dmg"
        } else {
            "Futureboard.Studio-2026.8.8-macos-arm64.dmg"
        };
        let universal = "Futureboard.Studio-2026.8.8-macos-universal.dmg";

        // Asset order must not decide the outcome.
        let all = with(vec![dmg(foreign), dmg(universal), dmg(native)]);
        assert_eq!(platform_asset(&all).unwrap().name, native);

        let no_native = with(vec![dmg(foreign), dmg(universal)]);
        assert_eq!(platform_asset(&no_native).unwrap().name, universal);

        // The wrong architecture alone is not an update.
        assert!(platform_asset(&with(vec![dmg(foreign)])).is_none());

        // Releases predating architecture-tagged names still resolve.
        let legacy = with(vec![dmg("Futureboard.Studio-2026.8.3-macos.dmg")]);
        assert_eq!(
            platform_asset(&legacy).unwrap().name,
            "Futureboard.Studio-2026.8.3-macos.dmg"
        );
    }

    /// An asset named the way the release workflow names them, for the platform
    /// this test binary is running on.
    fn versioned_release(tag: &str, name: &str, version: &str) -> GithubRelease {
        let asset = if cfg!(target_os = "windows") {
            format!("Futureboard.Studio-{version}-windows-x86_64-Setup.exe")
        } else if cfg!(target_os = "macos") {
            format!("Futureboard.Studio-{version}-macos-universal.dmg")
        } else {
            format!("Futureboard.Studio-{version}-x86_64.AppImage")
        };
        GithubRelease {
            tag_name: tag.to_string(),
            name: Some(name.to_string()),
            draft: false,
            prerelease: true,
            assets: vec![
                GithubAsset {
                    name: asset.clone(),
                    browser_download_url: "https://example.test/download".to_string(),
                    size: 1,
                    digest: Some("sha256:test".to_string()),
                },
                signature_for(&asset),
            ],
        }
    }

    /// A moving tag carries no version of its own, so it comes from the asset
    /// that would actually be installed.
    #[test]
    fn moving_tag_version_comes_from_the_asset_name() {
        let nightly = versioned_release("nightly", "Nightly v2026.8.3-nightly", "2026.8.3-nightly");
        assert_eq!(
            release_version(&nightly).unwrap(),
            Version::parse("2026.8.3-nightly").unwrap()
        );

        // 2026.8.9-beta1 shipped on `beta`. Before the asset fallback existed
        // this release resolved to `None` and was skipped by every channel.
        let beta = versioned_release(
            "beta",
            "Futureboard Studio 2026.8.9 Beta 1 — Hotfix",
            "2026.8.9-beta1",
        );
        assert_eq!(
            release_version(&beta).unwrap(),
            Version::parse("2026.8.9-beta1").unwrap()
        );
        assert!(release_matches_channel(&beta, UpdateChannel::Beta));
    }

    /// The release title is prose. "…2026.8.9 Beta 1" offers `2026.8.9`, which
    /// sorts *above* the `2026.8.9-beta1` in the installer, so trusting it would
    /// offer a user on the hotfix a permanent update to their own build.
    #[test]
    fn asset_name_outranks_a_prose_release_title() {
        let release = versioned_release(
            "beta",
            "Futureboard Studio 2026.8.9 Beta 1 — Hotfix",
            "2026.8.9-beta1",
        );
        let current = Version::parse("2026.8.9-beta1").unwrap();
        assert!(choose_release(vec![release], UpdateChannel::Beta, &current).is_none());
    }

    /// A version tag still wins outright, and a release whose version cannot be
    /// established anywhere is skipped rather than guessed at.
    #[test]
    fn version_tag_wins_and_an_unresolvable_release_is_skipped() {
        let tagged = versioned_release("v2026.8.9-beta1", "Anything at all", "2026.8.9-beta1");
        assert_eq!(
            release_version(&tagged).unwrap(),
            Version::parse("2026.8.9-beta1").unwrap()
        );

        let mut opaque = versioned_release("beta", "Futureboard Studio", "2026.8.9-beta1");
        opaque.assets[0].name = "installer.bin".to_string();
        assert!(release_version(&opaque).is_none());
    }

    #[test]
    fn stable_does_not_accept_mislabelled_prerelease_tag() {
        let candidate = release("2026.8.2-beta.1", "Beta", false);
        assert!(!release_matches_channel(&candidate, UpdateChannel::Stable));
        assert!(release_matches_channel(&candidate, UpdateChannel::Beta));
    }

    /// A 404 from the channel lookup resolves to an empty release list, which
    /// must read as "up to date" rather than propagating as a failure. The
    /// nightly channel asks for `releases/tags/nightly` by name, so it 404s
    /// until the first nightly of a cycle exists.
    #[test]
    fn a_channel_with_no_releases_offers_nothing() {
        let current = Version::parse("2026.8.1").unwrap();
        for channel in UpdateChannel::ALL {
            assert!(choose_release(Vec::new(), channel, &current).is_none());
        }
    }

    /// The updater must ask for the repository under its current name. The old
    /// `futureboard/Futureboard` path survives only on GitHub's rename
    /// redirect.
    #[test]
    fn the_releases_api_names_the_current_repository() {
        assert_eq!(
            RELEASES_API,
            "https://api.github.com/repos/futureboard/futureboard-studio/releases"
        );
    }

    /// Test-only signing key. It signs nothing outside these tests and is
    /// unrelated to the release key baked in through
    /// `FUTUREBOARD_UPDATE_PUBLIC_KEY`.
    fn test_signing_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32])
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn test_public_key_hex() -> String {
        hex(test_signing_key().verifying_key().as_bytes())
    }

    fn sign(asset_name: &str, digest: &[u8; 32]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        test_signing_key()
            .sign(&signed_message(asset_name, digest))
            .to_bytes()
    }

    const ASSET: &str = "Futureboard.Studio-2026.11.2-windows-x86_64-Setup.exe";

    #[test]
    fn a_genuine_signature_verifies_raw_or_hex() {
        let key = test_public_key_hex();
        let digest = [0x5a_u8; 32];
        let signature = sign(ASSET, &digest);
        assert!(verify_update_signature(Some(&key), ASSET, &digest, &signature).is_ok());
        let hex_signature = format!("{}\n", hex(&signature));
        assert!(
            verify_update_signature(Some(&key), ASSET, &digest, hex_signature.as_bytes()).is_ok()
        );
    }

    /// No key baked in means nothing can be verified, so every update is
    /// refused rather than trusted on the release host's word.
    #[test]
    fn a_build_without_a_key_rejects_every_update() {
        let digest = [1_u8; 32];
        let signature = sign(ASSET, &digest);
        assert!(verify_update_signature(None, ASSET, &digest, &signature).is_err());
        assert!(verify_update_signature(Some("  "), ASSET, &digest, &signature).is_err());
        assert!(verify_update_signature(Some("not-hex"), ASSET, &digest, &signature).is_err());
    }

    #[test]
    fn a_tampered_or_foreign_signature_is_rejected() {
        let key = test_public_key_hex();
        let digest = [2_u8; 32];
        let signature = sign(ASSET, &digest);

        // Different installer bytes.
        let mut other_digest = digest;
        other_digest[0] ^= 1;
        assert!(verify_update_signature(Some(&key), ASSET, &other_digest, &signature).is_err());

        // A genuine signature for another asset (older version, other
        // platform) cannot vouch for this one.
        let older = "Futureboard.Studio-2026.11.1-windows-x86_64-Setup.exe";
        assert!(verify_update_signature(Some(&key), older, &digest, &signature).is_err());

        // Signed by someone else's key.
        let attacker = ed25519_dalek::SigningKey::from_bytes(&[9_u8; 32]);
        let attacker_signature = {
            use ed25519_dalek::Signer;
            attacker.sign(&signed_message(ASSET, &digest)).to_bytes()
        };
        assert!(verify_update_signature(Some(&key), ASSET, &digest, &attacker_signature).is_err());

        // Missing, truncated or garbage signature bytes.
        assert!(verify_update_signature(Some(&key), ASSET, &digest, b"").is_err());
        assert!(verify_update_signature(Some(&key), ASSET, &digest, &signature[..63]).is_err());
        assert!(verify_update_signature(Some(&key), ASSET, &digest, &[0_u8; 64]).is_err());
    }

    /// A tag cannot lift a signed installer to a version its name does not
    /// carry: such a release is not offered, and download refuses it too.
    #[test]
    fn the_offered_version_must_match_the_signed_asset_name() {
        let current = Version::parse("2026.8.1").unwrap();
        let honest = versioned_release("v2026.8.9", "Release", "2026.8.9");
        let update = choose_release(vec![honest], UpdateChannel::Beta, &current).unwrap();
        assert!(asset_carries_offered_version(&update));

        let mut relabelled = versioned_release("v2026.9.0", "Release", "2026.8.2");
        relabelled.prerelease = false;
        assert!(choose_release(vec![relabelled.clone()], UpdateChannel::Beta, &current).is_none());

        // Download is still fail-closed for an update built by hand.
        let forged = AvailableUpdate {
            version: Version::parse("2026.9.0").unwrap(),
            channel: UpdateChannel::Beta,
            asset: relabelled.assets[0].clone(),
            signature: relabelled.assets[1].clone(),
        };
        assert!(!asset_carries_offered_version(&forged));
    }

    /// The signature travels as `<installer>.sig` in the same release, matched
    /// exactly. A release without one is skipped, so an older signed release
    /// is offered instead of a newer unverifiable one.
    #[test]
    fn a_release_without_its_signature_is_not_offered() {
        let current = Version::parse("2026.8.1").unwrap();
        let signed = versioned_release("v2026.8.9", "Release", "2026.8.9");
        let installer = signed.assets[0].name.clone();
        let update = choose_release(vec![signed], UpdateChannel::Beta, &current).unwrap();
        assert_eq!(update.signature.name, format!("{installer}.sig"));

        let mut unsigned = versioned_release("v2026.8.9", "Release", "2026.8.9");
        unsigned.assets.truncate(1);
        assert!(choose_release(vec![unsigned.clone()], UpdateChannel::Beta, &current).is_none());

        // Near-miss names are not the signature.
        let mut misnamed = unsigned.clone();
        misnamed
            .assets
            .push(signature_for(&installer.to_ascii_lowercase()));
        let mut suffixed = signature_for(&installer);
        suffixed.name.push_str(".txt");
        misnamed.assets.push(suffixed);
        assert!(choose_release(vec![misnamed], UpdateChannel::Beta, &current).is_none());

        let mut newer_unsigned = versioned_release("v2026.8.10", "Release", "2026.8.10");
        newer_unsigned.assets.truncate(1);
        let older_signed = versioned_release("v2026.8.9", "Release", "2026.8.9");
        let update = choose_release(
            vec![newer_unsigned, older_signed],
            UpdateChannel::Beta,
            &current,
        )
        .unwrap();
        assert_eq!(update.version, Version::parse("2026.8.9").unwrap());
    }

    /// Without a usable key the check reports updates as unavailable (an
    /// error the dialog shows), never "up to date" and never an offer, and it
    /// does so before any network request.
    #[test]
    fn a_build_without_a_key_offers_no_update() {
        for key in [None, Some(""), Some("not-a-key")] {
            let result = check_for_update_with_key(key, UpdateChannel::Stable, "2026.8.1");
            assert!(result.is_err(), "{key:?} must not report a check result");
        }
        assert_eq!(
            check_for_update_with_key(None, UpdateChannel::Stable, "2026.8.1").unwrap_err(),
            NO_KEY_ERROR
        );
        assert!(update_verifying_key(Some(&test_public_key_hex())).is_ok());
    }

    /// Download is fail-closed on its own too: an update refused before any
    /// network request (the URL is unreachable anyway) leaves no installer
    /// behind.
    #[test]
    fn download_without_a_key_stages_nothing() {
        if UPDATE_PUBLIC_KEY.is_some() {
            // A key baked into this test build would let the request proceed.
            return;
        }
        let current = Version::parse("2026.8.1").unwrap();
        let signed = versioned_release("v2026.8.9", "Release", "2026.8.9");
        let update = choose_release(vec![signed], UpdateChannel::Beta, &current).unwrap();
        let cache =
            std::env::temp_dir().join(format!("futureboard-updater-test-{}", std::process::id()));
        let result = download_update(&update, &cache, &|_, _| {});
        assert_eq!(result.unwrap_err(), NO_KEY_ERROR);
        let staged = cache
            .join("Updates")
            .join(update.version.to_string())
            .join(&update.asset.name);
        assert!(!staged.exists());
        assert!(!staged.with_extension("part").exists());
        let _ = fs::remove_dir_all(&cache);
    }

    #[test]
    fn channel_selection_uses_newest_eligible_release() {
        let releases = vec![
            release("v2026.8.2", "Stable", false),
            release("v2026.8.3-beta.1", "Beta", true),
        ];
        let current = Version::parse("2026.8.1").unwrap();
        let stable = choose_release(releases.clone(), UpdateChannel::Stable, &current).unwrap();
        let beta = choose_release(releases, UpdateChannel::Beta, &current).unwrap();
        assert_eq!(stable.version, Version::parse("2026.8.2").unwrap());
        assert_eq!(beta.version, Version::parse("2026.8.3-beta.1").unwrap());
    }
}
