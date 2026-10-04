//! Build and stage the standalone Audio Jam client.
//!
//! `FutureboardJam` is a much smaller thing to package than Studio, and the
//! difference is the point of this module rather than an accident of it. Studio
//! ships a CEF runtime, two sidecar processes it spawns from its own directory,
//! the APAK tools, and per-edition feature sets. The jam client ships one
//! executable: it hosts no plug-ins, spawns nothing, and has no editions —
//! there is no Professional jam, because a jam is a network client, not a
//! licensed feature.
//!
//! So this is deliberately not routed through [`crate::package`]. Reusing that
//! pipeline would mean threading "no CEF, no sidecars, no plugins, no edition"
//! through every stage of it, which is more conditional machinery than the
//! whole task is worth — and every one of those conditions is a way for a jam
//! package to fail on a rule that does not apply to it. What *is* shared is the
//! part worth sharing: artifact discovery from Cargo's own messages, and the
//! staging/publish dance that never leaves a half-written package behind.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::{Artifact, Message};

use crate::platform::{host_target, platform_folder};
use crate::staging;

/// The standalone client's package and binary.
pub const JAM_PACKAGE: &str = "jamsession";
pub const JAM_BINARY: &str = "FutureboardJam";

/// Cargo target directory.
///
/// The Community one, shared on purpose: the jam client depends on the same
/// `sphere_ui_components` and `sphere_directaudioengine` as the Community
/// Studio build, with the same features, so a third target directory would
/// recompile the entire workspace for no difference in output.
const TARGET_DIR: &str = "target/community";

pub struct JamOptions {
    pub profile: String,
    pub target: Option<String>,
    /// Root of the distributable tree (default `out`).
    pub out_root: PathBuf,
    /// Also stage debug symbols into `symbols/`.
    pub symbols: bool,
    /// Build only — do not stage anything into `out/`.
    pub build_only: bool,
}

/// Build, and unless `build_only`, stage. Returns the published directory, or
/// the executable's own path when only building.
pub fn run(options: &JamOptions) -> Result<PathBuf> {
    let target_triple = match &options.target {
        Some(target) => target.clone(),
        None => host_target().context("could not determine host target triple")?,
    };

    let executable = build(&options.profile, options.target.as_deref())?;
    eprintln!("[xtask] built executable: {}", executable.display());
    if options.build_only {
        return Ok(executable);
    }

    let platform = platform_folder(&target_triple);
    let staging_dir = staging_dir(&options.out_root, &options.profile, &platform);
    let final_dir = final_output_dir(&options.out_root, &options.profile, &platform);

    if staging_dir.exists() {
        std::fs::remove_dir_all(&staging_dir).with_context(|| {
            format!(
                "failed to clear staging directory {}",
                staging_dir.display()
            )
        })?;
    }
    std::fs::create_dir_all(&staging_dir)
        .with_context(|| format!("failed to create {}", staging_dir.display()))?;

    let binary = staging::stage_executable(&staging_dir, &executable)?;
    eprintln!("[xtask] staged {binary}");

    if options.symbols {
        for symbol in staging::stage_symbols(&staging_dir, &executable)? {
            eprintln!("[xtask] staged symbols: {symbol}");
        }
    }

    staging::publish(&staging_dir, &final_dir)?;
    Ok(final_dir)
}

/// Where a jam package lands.
///
/// One rule for every profile — `out/<profile>/jam/<platform>` — rather than
/// Studio's dev special case. Studio's layout drops the edition on `dev`
/// because a developer package has no edition to name; the jam client has no
/// edition on *any* profile, so there is nothing to drop and no reason for the
/// path to change shape with the profile.
pub fn final_output_dir(out_root: &Path, profile: &str, platform: &str) -> PathBuf {
    out_root.join(profile).join("jam").join(platform)
}

/// Staging directory for a jam package.
///
/// Prefixed so it can never collide with a Studio package staged for the same
/// platform and profile — the two are different trees and a shared temporary
/// directory would have one clearing the other's files mid-build.
pub fn staging_dir(out_root: &Path, profile: &str, platform: &str) -> PathBuf {
    out_root
        .join(".staging")
        .join(format!("jam-{platform}-{profile}"))
}

/// Build the client and return the executable path Cargo actually produced.
///
/// Discovered from `compiler-artifact` messages rather than guessed at
/// `target/<profile>/FutureboardJam`, for the same reason the Studio build does
/// it: the answer changes with the profile, the target triple and the target
/// directory, and a guess that is wrong stages nothing and says nothing useful
/// about why.
fn build(profile: &str, target: Option<&str>) -> Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(&cargo);
    command
        .arg("build")
        .arg("--message-format=json-render-diagnostics")
        .args(["--package", JAM_PACKAGE])
        .args(["--bin", JAM_BINARY])
        .args(["--profile", profile])
        .args(["--target-dir", TARGET_DIR]);
    crate::jobs::apply(&mut command, Some(profile), &[]);
    if let Some(target) = target {
        command.args(["--target", target]);
    }

    eprintln!(
        "[xtask] building {JAM_BINARY} (profile={profile}, target={})",
        target.unwrap_or("<host>")
    );
    command.stdout(Stdio::piped()).stderr(Stdio::inherit());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn `{cargo} build`"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("cargo produced no stdout stream"))?;

    let mut executables: BTreeMap<String, PathBuf> = BTreeMap::new();
    for message in Message::parse_stream(BufReader::new(stdout)) {
        let message = message.context("failed to parse a cargo JSON message")?;
        if let Message::CompilerArtifact(artifact) = message {
            if let Some((name, path)) = wanted_executable(&artifact) {
                executables.insert(name, path);
            }
        }
    }

    let status = child.wait().context("failed to wait on cargo build")?;
    if !status.success() {
        bail!("cargo build failed with {status}");
    }

    executables.remove(JAM_BINARY).ok_or_else(|| {
        anyhow!(
            "cargo build succeeded but emitted no executable artifact for `{JAM_BINARY}`; \
             is the `{JAM_PACKAGE}` package still producing a `[[bin]]` named `{JAM_BINARY}`?"
        )
    })
}

fn wanted_executable(artifact: &Artifact) -> Option<(String, PathBuf)> {
    let name = artifact.target.name.as_str();
    if name != JAM_BINARY
        || !artifact
            .target
            .kind
            .iter()
            .any(|kind| kind.as_str() == "bin")
    {
        return None;
    }
    artifact
        .executable
        .as_ref()
        .map(|path| (name.to_string(), PathBuf::from(path.as_std_path())))
}

#[cfg(test)]
mod tests {
    use super::{TARGET_DIR, final_output_dir, staging_dir};
    use std::path::Path;

    /// The jam client has no editions, so its path never grows one — including
    /// on `dev`, where Studio's layout drops the edition segment instead.
    #[test]
    fn the_output_path_has_the_same_shape_on_every_profile() {
        let out = Path::new("out");
        assert_eq!(
            final_output_dir(out, "dev", "windows-x64"),
            Path::new("out/dev/jam/windows-x64")
        );
        assert_eq!(
            final_output_dir(out, "release", "windows-x64"),
            Path::new("out/release/jam/windows-x64")
        );
    }

    /// A jam package and a Studio package staged for the same platform and
    /// profile must not share a temporary directory: publishing clears it, and
    /// one build would delete the other's staged files mid-flight.
    #[test]
    fn staging_never_collides_with_a_studio_package() {
        let out = Path::new("out");
        let jam = staging_dir(out, "release", "windows-x64");
        let studio = crate::staging::staging_dir(
            out,
            "release",
            crate::platform::Edition::Community,
            "windows-x64",
        );
        assert_ne!(jam, studio);
        assert!(
            jam.to_string_lossy().contains("jam-"),
            "the prefix is what keeps them apart"
        );
    }

    /// Sharing the Community target directory is what stops a jam build from
    /// recompiling the whole workspace: the two builds want the same crates
    /// with the same features.
    #[test]
    fn the_build_shares_the_community_target_directory() {
        assert_eq!(TARGET_DIR, crate::platform::Edition::Community.target_dir());
    }
}
