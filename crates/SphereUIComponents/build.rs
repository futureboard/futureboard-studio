//! Bakes compile-time config that this crate reads with `option_env!`.
//!
//! `option_env!` resolves where the code is *compiled*, so this has to live in
//! the crate that owns the readers (`auth.rs`, `tone3000.rs`). It used to be
//! baked by the studio binary's build script, which was correct while those
//! modules were `include!`d into that binary; once they moved here, the
//! studio's `cargo:rustc-env` stopped reaching them.
//!
//! Source precedence matches the studio's own bake: an explicit build
//! environment value wins (CI / distribution), otherwise the workspace `.env`
//! is read so a plain `cargo build` produces a working binary.
//!
//! Two values are baked:
//! - `FUTUREBOARD_AUTH_API_URL` — public account-service origin, not a secret.
//! - `FUTUREBOARD_TONE3000_API_KEY` — TONE3000 partner Bearer key for Rodhareist
//!   NAM A2 catalog fetch. Native-only; it never crosses the CEF editor bridge.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FUTUREBOARD_AUTH_API_URL");
    println!("cargo:rerun-if-env-changed=FUTUREBOARD_TONE3000_API_KEY");
    println!("cargo:rerun-if-env-changed=TONE3000_API_KEY");

    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    // crates/SphereUIComponents → workspace root `.env` (absolute so the
    // working directory never matters).
    let dotenv_path = manifest_dir
        .join("../..")
        .join(".env")
        .canonicalize()
        .unwrap_or_else(|_| manifest_dir.join("../../.env"));
    println!("cargo:rerun-if-changed={}", dotenv_path.display());

    if let Some(url) = resolve_auth_api_url(&dotenv_path) {
        println!("cargo:rustc-env=FUTUREBOARD_AUTH_API_URL={url}");
    }
    if let Some(key) = resolve_tone3000_api_key(&dotenv_path) {
        println!("cargo:rustc-env=FUTUREBOARD_TONE3000_API_KEY={key}");
    }
}

/// Absent rather than guessed: `auth::auth_configured()` reports false and the
/// UI disables sign-in, which is better than pointing a real sign-in at a
/// placeholder host.
fn resolve_auth_api_url(dotenv_path: &Path) -> Option<String> {
    std::env::var("FUTUREBOARD_AUTH_API_URL")
        .ok()
        .or_else(|| read_dotenv_value(dotenv_path, "AUTH_API_URL"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_tone3000_api_key(dotenv_path: &Path) -> Option<String> {
    std::env::var("FUTUREBOARD_TONE3000_API_KEY")
        .ok()
        .or_else(|| std::env::var("TONE3000_API_KEY").ok())
        .or_else(|| read_dotenv_value(dotenv_path, "FUTUREBOARD_TONE3000_API_KEY"))
        .or_else(|| read_dotenv_value(dotenv_path, "TONE3000_API_KEY"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && !value.contains('\n') && !value.contains('\r'))
}

fn read_dotenv_value(path: &Path, key: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    contents.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (k, v) = line.split_once('=')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}
