//! `build-info.json` generation.
//!
//! Collects versioning, git and toolchain metadata. Git and toolchain lookups
//! are best-effort: a missing `git` or detached environment yields `null`
//! rather than failing the whole package operation.

use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

/// Bump when the on-disk shape of `build-info.json` changes. 2: one package,
/// so `edition` became `professional`.
const SCHEMA_VERSION: u32 = 2;

/// Serializable contents of `build-info.json`. `camelCase` to match the shape
/// requested by the packaging spec and the wider JS tooling.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub schema_version: u32,
    pub application: String,
    pub binary: String,
    /// Runtime sidecar executables shipped beside `binary`.
    pub sidecars: Vec<String>,
    /// Whether the Professional code is compiled in. It is unlocked at runtime
    /// by a licence, so this says what the package *can* do, not what a user
    /// gets.
    pub professional: bool,
    pub profile: String,
    pub target: String,
    pub platform: String,
    pub version: String,
    pub git_commit: Option<String>,
    pub git_dirty: Option<bool>,
    pub built_at_utc: String,
    pub rustc_version: Option<String>,
    pub cargo_version: Option<String>,
}

/// Inputs the caller already knows, kept separate from the values this module
/// discovers itself (git / toolchain / clock).
pub struct MetadataInputs<'a> {
    pub binary: &'a str,
    pub sidecars: &'a [String],
    pub professional: bool,
    pub profile: &'a str,
    pub target: &'a str,
    pub platform: &'a str,
    pub version: &'a str,
}

impl BuildInfo {
    /// Assemble build metadata, resolving git and toolchain fields best-effort.
    pub fn collect(inputs: &MetadataInputs<'_>) -> Self {
        BuildInfo {
            schema_version: SCHEMA_VERSION,
            application: "Futureboard Studio".to_string(),
            binary: inputs.binary.to_string(),
            sidecars: inputs.sidecars.to_vec(),
            professional: inputs.professional,
            profile: inputs.profile.to_string(),
            target: inputs.target.to_string(),
            platform: inputs.platform.to_string(),
            version: inputs.version.to_string(),
            git_commit: git_commit(),
            git_dirty: git_dirty(),
            built_at_utc: chrono::Utc::now().to_rfc3339(),
            rustc_version: tool_version("rustc"),
            cargo_version: tool_version("cargo"),
        }
    }

    /// Pretty-printed JSON, newline-terminated for clean diffs.
    pub fn to_json(&self) -> Result<String> {
        let mut json =
            serde_json::to_string_pretty(self).context("failed to serialize build-info")?;
        json.push('\n');
        Ok(json)
    }
}

/// `git rev-parse HEAD`, or `None` when git/metadata is unavailable.
fn git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!commit.is_empty()).then_some(commit)
}

/// Whether the working tree has uncommitted changes. `None` when git is
/// unavailable (so consumers can distinguish "clean" from "unknown").
fn git_dirty() -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(!text.trim().is_empty())
}

/// First line of `<tool> --version` (e.g. `rustc 1.xx.0 (...)`), or `None`.
fn tool_version(tool: &str) -> Option<String> {
    let output = Command::new(tool).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BuildInfo {
        BuildInfo {
            schema_version: SCHEMA_VERSION,
            application: "Futureboard Studio".to_string(),
            binary: "FutureboardNative.exe".to_string(),
            sidecars: vec![
                "FutureboardPluginHostX64.exe".to_string(),
                "FutureboardPluginScanner.exe".to_string(),
            ],
            professional: true,
            profile: "release".to_string(),
            target: "x86_64-pc-windows-msvc".to_string(),
            platform: "windows-x64".to_string(),
            version: "2026.7.2".to_string(),
            git_commit: Some("abc123".to_string()),
            git_dirty: Some(false),
            built_at_utc: "2026-07-19T00:00:00+00:00".to_string(),
            rustc_version: Some("rustc 1.90.0".to_string()),
            cargo_version: None,
        }
    }

    #[test]
    fn serializes_to_camel_case_and_round_trips() {
        let json = sample().to_json().unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["schemaVersion"], 2);
        assert_eq!(value["application"], "Futureboard Studio");
        assert_eq!(value["binary"], "FutureboardNative.exe");
        assert_eq!(value["sidecars"][0], "FutureboardPluginHostX64.exe");
        assert_eq!(value["sidecars"][1], "FutureboardPluginScanner.exe");
        assert_eq!(value["professional"], true);
        assert!(value.get("edition").is_none());
        assert_eq!(value["platform"], "windows-x64");
        assert_eq!(value["gitCommit"], "abc123");
        assert_eq!(value["gitDirty"], false);
        // Optional/unknown values are explicit null, not omitted.
        assert!(value.get("cargoVersion").unwrap().is_null());
    }

    #[test]
    fn json_is_valid_and_newline_terminated() {
        let json = sample().to_json().unwrap();
        assert!(json.ends_with('\n'));
        serde_json::from_str::<serde_json::Value>(&json).expect("must be valid JSON");
    }
}
