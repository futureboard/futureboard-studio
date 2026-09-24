//! Embeds Windows icon, application manifest, and version resources from `apps/shared/`.

use std::path::{Path, PathBuf};

fn main() {
    bundle_crashpad_handler();

    println!("cargo:rerun-if-changed=../../../packages/shared/app/windows/app.rc");
    println!("cargo:rerun-if-changed=../../../packages/shared/app/windows/app.manifest");
    println!("cargo:rerun-if-changed=../../../packages/shared/app/icons/icon.ico");
    println!("cargo:rerun-if-changed=../../../packages/shared/app/icons/fbproj.ico");
    println!("cargo:rerun-if-changed=../../../.discordrpcsecret");

    stage_professional_sources();
    download_onnxruntime();

    // Optional override for the Discord application id. Futureboard's own is
    // compiled into `sphere_discord_rpc` (`DEFAULT_APPLICATION_ID`), so this
    // only matters for a fork or a build that targets a different Discord
    // application. An explicit build environment value wins over the file.
    if std::env::var_os("FUTUREBOARD_DISCORD_CLIENT_ID").is_none() {
        if let Ok(application_id) = std::fs::read_to_string("../../../.discordrpcsecret") {
            let application_id = application_id.trim();
            if !application_id.is_empty()
                && application_id
                    .chars()
                    .all(|character| character.is_ascii_digit())
            {
                println!("cargo:rustc-env=FUTUREBOARD_DISCORD_CLIENT_ID={application_id}");
            }
        }
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // `libcef` (and the staged ONNX Runtime) ship beside the binary in the
    // packaged tree, so the loader must search the executable's own directory.
    // Windows already searches it; ELF/Mach-O need an explicit run path.
    match target_os.as_str() {
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN"),
        "macos" => println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path"),
        _ => {}
    }
    if target_os == "windows" {
        embed_resource::compile(
            "../../../packages/shared/app/windows/app.rc",
            embed_resource::NONE,
        )
        .manifest_required()
        .unwrap();
    }
}

/// Keep mobile/unknown targets buildable while requiring a handler for the
/// desktop targets that actually launch one. The bundled Crashpad crate uses
/// an in-process handler on iOS and does not have an external executable to
/// copy there.
fn bundle_crashpad_handler() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let uses_external_handler = matches!(
        target_os.as_str(),
        "android" | "linux" | "macos" | "windows"
    );
    if !uses_external_handler {
        println!(
            "cargo:warning=Crashpad external handler bundling skipped for target OS `{target_os}`"
        );
        return;
    }

    if let Err(error) = crashpad_handler_bundler::bundle() {
        panic!(
            "failed to bundle Crashpad handler for `{target_os}`: {error}. \
             Set CRASHPAD_HANDLER to a target-native handler or use the matching \
             Crashpad build strategy."
        );
    }
}

/// Default ONNX Runtime release fetched for the real MDX-NET stem backend.
/// Override with `FUTUREBOARD_ORT_VERSION`.
// `ort 2.0.0-rc.11` requires an ONNX Runtime 1.23.x shared library.
// Keep the default aligned with the Rust binding; newer runtime releases can
// be selected explicitly once the binding is upgraded with them.
const ORT_DEFAULT_VERSION: &str = "1.23.2";

/// Download the ONNX Runtime shared library from the microsoft/onnxruntime
/// GitHub release and place it next to the built binary, so the Stem Extractor
/// can load it at runtime (`{appdir}/onnxruntime.dll` · `libonnxruntime.so` ·
/// `libonnxruntime.dylib`).
///
/// Only runs with `--features stem-onnx`. Failures are non-fatal: the app still
/// builds and simply falls back to the spectral stub at runtime. Set
/// `FUTUREBOARD_ORT_SKIP_DOWNLOAD=1` to skip (e.g. offline / air-gapped builds).
fn download_onnxruntime() {
    println!("cargo:rerun-if-env-changed=FUTUREBOARD_ORT_VERSION");
    println!("cargo:rerun-if-env-changed=FUTUREBOARD_ORT_SKIP_DOWNLOAD");

    if std::env::var_os("CARGO_FEATURE_STEM_ONNX").is_none() {
        return;
    }
    if std::env::var_os("FUTUREBOARD_ORT_SKIP_DOWNLOAD").is_some() {
        println!("cargo:warning=ONNX Runtime download skipped (FUTUREBOARD_ORT_SKIP_DOWNLOAD set)");
        return;
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let cuda = std::env::var_os("CARGO_FEATURE_STEM_CUDA").is_some();
    let directml = std::env::var_os("CARGO_FEATURE_STEM_DIRECTML").is_some();

    let version = std::env::var("FUTUREBOARD_ORT_VERSION")
        .unwrap_or_else(|_| ORT_DEFAULT_VERSION.to_string());

    // Where the binary ends up: OUT_DIR is target/<profile>/build/<pkg>/out.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .unwrap_or(out_dir.clone());

    let (lib_name, dest_name) = match target_os.as_str() {
        "windows" => ("onnxruntime.dll", "onnxruntime.dll"),
        "macos" => ("libonnxruntime", "libonnxruntime.dylib"),
        "linux" => ("libonnxruntime.so", "libonnxruntime.so"),
        other => {
            println!("cargo:warning=ONNX Runtime auto-download unsupported for OS `{other}`");
            return;
        }
    };
    let dest = profile_dir.join(dest_name);
    let version_marker = profile_dir.join(format!("{dest_name}.version"));
    if dest.is_file()
        && std::fs::read_to_string(&version_marker)
            .map(|cached| cached.trim() == version)
            .unwrap_or(false)
    {
        return; // Cached from a previous build of this exact runtime version.
    }

    let (platform, ext) = match (target_os.as_str(), target_arch.as_str()) {
        ("windows", "x86_64") => ("win-x64", "zip"),
        ("windows", "aarch64") => ("win-arm64", "zip"),
        ("linux", "x86_64") => ("linux-x64", "tgz"),
        ("linux", "aarch64") => ("linux-aarch64", "tgz"),
        // universal2 covers both Apple Silicon and Intel.
        ("macos", _) => ("osx-universal2", "tgz"),
        _ => {
            println!(
                "cargo:warning=ONNX Runtime auto-download unsupported for {target_os}/{target_arch}"
            );
            return;
        }
    };
    // Preferred assets in priority order. DirectML/GPU packages only exist for
    // x64 desktop; a CPU package is always the final fallback. GitHub currently
    // ships DirectML only via NuGet, so the DirectML candidate typically 404s
    // and we fall back to CPU (the DirectML EP then degrades to CPU at runtime).
    let mut assets: Vec<String> = Vec::new();
    if directml && platform == "win-x64" {
        assets.push(format!("onnxruntime-{platform}-directml-{version}.{ext}"));
    }
    if cuda && matches!(platform, "win-x64" | "linux-x64") {
        assets.push(format!("onnxruntime-{platform}-gpu-{version}.{ext}"));
    }
    assets.push(format!("onnxruntime-{platform}-{version}.{ext}"));

    for asset in &assets {
        let url = format!(
            "https://github.com/microsoft/onnxruntime/releases/download/v{version}/{asset}"
        );
        println!("cargo:warning=Downloading ONNX Runtime {version} ({asset})...");
        let bytes = match http_get(&url) {
            Ok(bytes) => bytes,
            Err(err) => {
                println!("cargo:warning=  {asset} unavailable ({err})");
                continue;
            }
        };
        let extracted = if ext == "zip" {
            extract_lib_from_zip(&bytes, lib_name)
        } else {
            extract_lib_from_tgz(&bytes, lib_name)
        };
        match extracted {
            Some(lib) => match std::fs::write(&dest, lib) {
                Ok(()) => {
                    let _ = std::fs::write(&version_marker, &version);
                    println!("cargo:warning=ONNX Runtime staged at {}", dest.display());
                    return;
                }
                Err(err) => println!("cargo:warning=Could not write {}: {err}", dest.display()),
            },
            None => println!("cargo:warning=  {lib_name} not found inside {asset}"),
        }
    }

    println!(
        "cargo:warning=ONNX Runtime not staged; Stem Extractor will use the spectral stub. \
         Place {dest_name} beside the app or set ORT_DYLIB_PATH to enable real MDX-NET."
    );
}

/// GET `url` into memory, following redirects (GitHub → object storage).
fn http_get(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("request error: {e}"))?;
    let mut buf = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("read error: {e}"))?;
    Ok(buf)
}

/// Extract the regular file whose name matches `lib_name` from a zip archive.
fn extract_lib_from_zip(bytes: &[u8], lib_name: &str) -> Option<Vec<u8>> {
    use std::io::{Cursor, Read};
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).ok()?;
        if !file.is_file() {
            continue;
        }
        let name = file.name().rsplit('/').next().unwrap_or("").to_string();
        if lib_matches(&name, lib_name) {
            let mut out = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut out).ok()?;
            return Some(out);
        }
    }
    None
}

/// Extract the regular file whose name matches `lib_name` from a gzip+tar
/// archive.
fn extract_lib_from_tgz(bytes: &[u8], lib_name: &str) -> Option<Vec<u8>> {
    use std::io::{Cursor, Read};
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().ok()? {
        let mut entry = entry.ok()?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        if lib_matches(&name, lib_name) {
            let mut out = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut out).ok()?;
            return Some(out);
        }
    }
    None
}

/// Whether an archive entry file name is the ONNX Runtime library. Handles
/// versioned unix sonames (`libonnxruntime.so.1.27.1`, `libonnxruntime.1.27.1.dylib`).
fn lib_matches(entry_name: &str, lib_name: &str) -> bool {
    if entry_name == lib_name {
        return true;
    }
    match lib_name {
        "libonnxruntime.so" => entry_name.starts_with("libonnxruntime.so"),
        "libonnxruntime" => {
            entry_name.starts_with("libonnxruntime") && entry_name.ends_with(".dylib")
        }
        _ => false,
    }
}

/// Stage private implementation files only when Cargo is compiling the
/// Professional Edition. `include!` cannot accept their crate-level `//!` comments
/// inside the application's bridge module, so those comments become ordinary
/// comments in the generated copies.
fn stage_professional_sources() {
    // The account endpoint, the activation endpoint, and the license signing key
    // are baked in via `option_env!`, from the build environment or the repo
    // `.env`. Rebuild when any of them changes so a build never keeps stale
    // config and never silently ships without it.
    //
    // The activation endpoint used to be described here as a constant in
    // `license.rs`. It is not — it is `option_env!("FUTUREBOARD_LICENSE_API_URL")`,
    // and this script never supplied it. The only thing that ever did was an
    // ambient `FUTUREBOARD_LICENSE_API_URL` export in a maintainer's shell, so
    // builds on that machine worked while every build without it — CI, a fresh
    // clone, another machine — silently produced a Professional binary whose
    // Activate button is disabled ("Activation is not configured for this
    // build"). Reading it from `.env` here removes that dependency on one
    // developer's environment.
    println!("cargo:rerun-if-changed=../../../.env");
    println!("cargo:rerun-if-env-changed=FUTUREBOARD_LICENSE_PUBLIC_KEY");
    println!("cargo:rerun-if-env-changed=FUTUREBOARD_AUTH_API_URL");
    println!("cargo:rerun-if-env-changed=FUTUREBOARD_LICENSE_API_URL");
    // The EULA is embedded (include_str!) from the staged copies below; rebuild
    // when the source text changes.
    println!("cargo:rerun-if-changed=../../../crates/ExclusiveEdition/assets/EULA.EN.txt");
    println!("cargo:rerun-if-changed=../../../crates/ExclusiveEdition/assets/EULA.TH.txt");

    if std::env::var_os("CARGO_FEATURE_PROFESSIONAL").is_none() {
        return;
    }

    bake_service_config();

    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let source_dir = manifest_dir.join("../../../crates/ExclusiveEdition/src");
    let assets_dir = manifest_dir.join("../../../crates/ExclusiveEdition/assets");
    let output_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"))
        .join("futureboard-professional");

    std::fs::create_dir_all(&output_dir).expect("failed to create Professional Edition output dir");

    stage_professional_source(&source_dir, &output_dir, "license.rs");
    stage_professional_source(&source_dir, &output_dir, "license_activation_dialog.rs");
    stage_professional_source(&source_dir, &output_dir, "eula.rs");
    stage_professional_source(&source_dir, &output_dir, "eula_dialog.rs");
    stage_professional_source(&source_dir, &output_dir, "updates.rs");

    // The EULA text is embedded into the binary. Copy it beside the staged
    // source so `include_str!(concat!(env!("OUT_DIR"), ...))` finds it.
    copy_professional_asset(&assets_dir, &output_dir, "EULA.EN.txt");
    copy_professional_asset(&assets_dir, &output_dir, "EULA.TH.txt");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        stage_professional_source(&source_dir, &output_dir, "asio.rs");
        // The ASIO logo the About window shows beside the licensed engine is
        // embedded (include_bytes!) from this staged copy, like the EULA text.
        copy_professional_asset_as(
            &assets_dir,
            &output_dir,
            "logo/asio-logo.png",
            "asio-logo.png",
        );
    }
}

/// As [`copy_professional_asset`], for an asset that lives in a subdirectory of
/// `assets/`: it is staged flat under `staged_name` so the staged source can
/// embed it with one `concat!(env!("OUT_DIR"), ...)` path.
fn copy_professional_asset_as(
    assets_dir: &Path,
    output_dir: &Path,
    relative_path: &str,
    staged_name: &str,
) {
    let source_path = assets_dir.join(relative_path);
    println!("cargo:rerun-if-changed={}", source_path.display());
    std::fs::copy(&source_path, output_dir.join(staged_name)).unwrap_or_else(|error| {
        panic!(
            "Professional Edition asset is required for --features professional: {}: {error}",
            source_path.display()
        )
    });
}

/// Copy an Professional Edition asset verbatim into the staging directory so the
/// staged source can embed it with `include_str!`.
fn copy_professional_asset(assets_dir: &Path, output_dir: &Path, file_name: &str) {
    let source_path = assets_dir.join(file_name);
    println!("cargo:rerun-if-changed={}", source_path.display());
    std::fs::copy(&source_path, output_dir.join(file_name)).unwrap_or_else(|error| {
        panic!(
            "Professional Edition asset is required for --features professional: {}: {error}",
            source_path.display()
        )
    });
}

/// Bake the account endpoint and the license signing key for `option_env!` in
/// the staged private sources. Source precedence: an explicit build-environment
/// value wins (CI/distribution), otherwise the repo `.env` is read so a plain
/// `cargo build` produces a working Professional build. An *invalid* environment
/// override is ignored with a warning so a stale shell export of the issue
/// secret cannot silence a correct `.env` public key.
///
/// Both values baked here are public by construction:
/// - the account API URL, which the app's sign-in opens in a browser;
/// - the Ed25519 **public** key licenses verify against.
///
/// The activation service URL is *not* configurable and is not read here — it is
/// a constant in `license.rs`, so no build can point licensing elsewhere.
///
/// SECURITY: no secret is ever read or emitted here. The `.env` also carries
/// service-role and issuing secrets, and any of those in a shipped desktop
/// binary would be a credential handed to every customer. The client needs none
/// of them: it proves who it is with the user's own session, and verifies
/// licenses with a public key.
fn bake_service_config() {
    let dotenv_path = {
        let manifest_dir = PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
        );
        // apps/native/studio → workspace root `.env` (absolute so CWD never matters).
        manifest_dir
            .join("../../..")
            .join(".env")
            .canonicalize()
            .unwrap_or_else(|_| manifest_dir.join("../../../.env"))
    };
    println!("cargo:rerun-if-changed={}", dotenv_path.display());
    let dotenv = read_dotenv(&dotenv_path);

    let resolve = |build_key: &str, dotenv_key: &str| -> Option<String> {
        std::env::var(build_key)
            .ok()
            .or_else(|| {
                dotenv
                    .iter()
                    .find(|(k, _)| k == dotenv_key)
                    .map(|(_, v)| v.clone())
            })
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };

    // FUTUREBOARD_AUTH_API_URL is baked by `sphere_ui_components`'s own build
    // script now: `option_env!` resolves in the crate that compiles the code,
    // and the account layer moved there.

    // The activation service. Without it `license::activation_endpoint()` is
    // `None`, the dialog disables Activate, and a paying customer is told
    // "Activation is not configured for this build" — which is exactly what
    // shipped before this was baked at all.
    //
    // A release build must not be pointed at a plaintext host: the license key
    // is sent to this URL. `license.rs` enforces the same rule at runtime; this
    // just refuses to bake a value that would be ignored there anyway.
    match resolve("FUTUREBOARD_LICENSE_API_URL", "LICENSE_API_URL") {
        Some(url) if is_acceptable_endpoint(&url) => {
            println!("cargo:rustc-env=FUTUREBOARD_LICENSE_API_URL={url}");
        }
        Some(url) => {
            println!(
                "cargo:warning=ignoring LICENSE_API_URL {url:?}: activation requires https \
                 (or a debug-only http://127.0.0.1 / http://localhost endpoint). \
                 This build cannot activate a license."
            );
        }
        None => {
            println!(
                "cargo:warning=LICENSE_API_URL is not set, so this Professional build \
                 CANNOT ACTIVATE A LICENSE (the dialog will say \"Activation is not \
                 configured for this build\"). Set FUTUREBOARD_LICENSE_API_URL or \
                 LICENSE_API_URL in {} — for Futureboard's own service that is \
                 https://avtlic.futureboard.studio/v1",
                dotenv_path.display()
            );
        }
    }

    let key = resolve_license_public_key(&dotenv);
    match key {
        Some(key) => {
            // Prefix only — full key is public anyway, but keeps logs compact.
            let prefix: String = key.chars().take(16).collect();
            println!("cargo:warning=baking license public key {prefix}…");
            println!("cargo:rustc-env=FUTUREBOARD_LICENSE_PUBLIC_KEY={key}");
        }
        None => {
            panic!(
                "Professional Edition requires a valid Ed25519 LICENSE_PUBLIC_KEY \
                 (64 hex chars, curve point). Set FUTUREBOARD_LICENSE_PUBLIC_KEY or \
                 LICENSE_PUBLIC_KEY in {}. \
                 `licensetool pubkey -key <signing.key>` prints the correct value. \
                 Do NOT paste ACTIVATION_ISSUE_SECRET / issue.secret — that is a \
                 different 64-hex token and produces \
                 \"Activation service is not configured for this build\".",
                dotenv_path.display()
            );
        }
    }
}

/// Mirrors `license.rs::is_acceptable_endpoint`: TLS is mandatory, and only a
/// debug build may talk to a plaintext loopback service. Kept in step with it
/// so a value that bakes cleanly is one the runtime will actually accept.
fn is_acceptable_endpoint(url: &str) -> bool {
    if url.starts_with("https://") {
        return true;
    }
    // `debug_assertions` here describes *this* build script, not the target
    // profile, so also accept loopback when an explicit debug profile is being
    // built — the runtime check is the authority either way.
    let debug_profile = std::env::var("PROFILE").as_deref() == Ok("debug");
    (cfg!(debug_assertions) || debug_profile)
        && (url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost"))
}

/// Prefer a valid env override, otherwise a valid `.env` value. Invalid hex /
/// non-curve values are skipped so a mistaken shell export cannot brick the build
/// when `.env` is correct.
fn resolve_license_public_key(dotenv: &[(String, String)]) -> Option<String> {
    let candidates = [
        (
            "FUTUREBOARD_LICENSE_PUBLIC_KEY",
            std::env::var("FUTUREBOARD_LICENSE_PUBLIC_KEY")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        ),
        (
            "LICENSE_PUBLIC_KEY (.env)",
            dotenv
                .iter()
                .find(|(k, _)| k == "LICENSE_PUBLIC_KEY")
                .map(|(_, v)| v.trim().to_string())
                .filter(|value| !value.is_empty()),
        ),
    ];

    for (source, value) in candidates {
        let Some(value) = value else {
            continue;
        };
        match validate_license_public_key(&value) {
            Ok(()) => return Some(value),
            Err(reason) => {
                println!("cargo:warning=ignoring license public key from {source}: {reason}");
            }
        }
    }
    None
}

/// Accept only what `ed25519-dalek::VerifyingKey` will accept at runtime. A seed
/// or issue secret is 64 hex chars too — and is exactly what shows up as
/// "Activation service is not configured for this build" if we bake it.
fn validate_license_public_key(hex: &str) -> Result<(), String> {
    let bytes = hex_decode_32(hex).ok_or_else(|| {
        format!(
            "expected 64 hex characters, got {} chars",
            hex.chars().count()
        )
    })?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes)
        .map(|_| ())
        .map_err(|error| {
            format!(
                "not a valid Ed25519 public key ({error}); \
                 often this is issue.secret / ACTIVATION_ISSUE_SECRET pasted by mistake"
            )
        })
}

fn hex_decode_32(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out[index] = ((high << 4) | low) as u8;
    }
    Some(out)
}

/// Minimal `KEY=VALUE` parser for the repo `.env`. Ignores blanks and `#`
/// comments; does not expand or unquote — the values used here are plain tokens.
fn read_dotenv(path: &Path) -> Vec<(String, String)> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn stage_professional_source(source_dir: &Path, output_dir: &Path, file_name: &str) {
    let source_path = source_dir.join(file_name);
    println!("cargo:rerun-if-changed={}", source_path.display());

    let source = std::fs::read_to_string(&source_path).unwrap_or_else(|error| {
        panic!(
            "Professional Edition source is required for --features professional: {}: {error}",
            source_path.display()
        )
    });
    let staged = source
        .lines()
        .map(|line| {
            line.strip_prefix("//!")
                .map_or_else(|| line.to_owned(), |comment| format!("//{comment}"))
        })
        .collect::<Vec<_>>()
        .join("\n");

    std::fs::write(output_dir.join(file_name), staged)
        .unwrap_or_else(|error| panic!("failed to stage {file_name}: {error}"));
}
