//! The 32-bit plug-in bridge shipped in 64-bit Windows packages.
//!
//! A 64-bit process cannot load a 32-bit module, so plug-ins built for 32-bit
//! Windows run in `FutureboardPluginHostX86.exe` and are scanned by
//! `FutureboardPluginScannerX86.exe`. Both are the `sphere-plugin-host`
//! sidecars built for `i686-pc-windows-msvc` into a target directory of their
//! own, then staged under their 32-bit names beside the 64-bit ones. The
//! Studio finds them next to itself
//! (`plugin_host_client::resolve_host_exe_for`, `locate_x86_scanner_binary`).

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::Message;

use crate::jobs;

/// The target the bridge is built for.
pub const X86_TARGET: &str = "i686-pc-windows-msvc";

/// Kept apart from `target/studio`: another target triple, and the static C
/// runtime `.cargo/config.toml` sets for it.
const X86_TARGET_DIR: &str = "target/x86";

/// `(Cargo binary, staged file name)` for each 32-bit sidecar.
pub const X86_SIDECARS: &[(&str, &str)] = &[
    ("FutureboardPluginHostX64", "FutureboardPluginHostX86.exe"),
    (
        "FutureboardPluginScanner",
        "FutureboardPluginScannerX86.exe",
    ),
];

const FEATURES: &str = "sphere-plugin-host/plugin-host-bin,sphere-plugin-host/plugin-scanner-bin";

/// Whether a package for `target_triple` ships the 32-bit bridge: 64-bit
/// Windows only. Nothing else has 32-bit plug-ins to load.
pub fn applies_to(target_triple: &str) -> bool {
    target_triple == "x86_64-pc-windows-msvc"
}

/// Build the 32-bit sidecars for `profile` and return each executable with
/// the file name it is staged under.
pub fn build(workspace: &Path, profile: &str) -> Result<Vec<(PathBuf, &'static str)>> {
    ensure_target_installed()?;
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let target_dir = workspace.join(X86_TARGET_DIR);
    let mut command = Command::new(&cargo);
    command
        .current_dir(workspace)
        .env("CARGO_TARGET_DIR", &target_dir)
        .arg("build")
        .arg("--message-format=json-render-diagnostics")
        .args(["--package", "sphere-plugin-host"])
        .args(["--profile", profile])
        .args(["--target", X86_TARGET])
        .arg("--target-dir")
        .arg(&target_dir)
        .args(["--features", FEATURES]);
    for (bin, _) in X86_SIDECARS {
        command.args(["--bin", bin]);
    }
    jobs::apply(&mut command, Some(profile), &[]);
    eprintln!(
        "[xtask] building the 32-bit plug-in bridge (profile={profile}, target={X86_TARGET})"
    );

    command.stdout(Stdio::piped()).stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn `{cargo} build` for {X86_TARGET}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("cargo produced no stdout stream"))?;
    let mut executables: BTreeMap<String, PathBuf> = BTreeMap::new();
    for message in Message::parse_stream(BufReader::new(stdout)) {
        let message = message.context("failed to parse a cargo JSON message")?;
        if let Message::CompilerArtifact(artifact) = message {
            let name = artifact.target.name.as_str();
            let is_bin = artifact
                .target
                .kind
                .iter()
                .any(|kind| kind.as_str() == "bin");
            if is_bin && X86_SIDECARS.iter().any(|(bin, _)| *bin == name) {
                if let Some(path) = artifact.executable.as_ref() {
                    executables.insert(name.to_string(), PathBuf::from(path.as_std_path()));
                }
            }
        }
    }
    let status = child.wait().context("failed to wait on cargo build")?;
    if !status.success() {
        bail!("cargo build of the 32-bit plug-in bridge failed with {status}");
    }
    X86_SIDECARS
        .iter()
        .map(|(bin, staged)| {
            executables
                .remove(*bin)
                .map(|path| (path, *staged))
                .ok_or_else(|| {
                    anyhow!(
                        "cargo build emitted no {X86_TARGET} executable for `{bin}`; \
                         the package ships it as {staged}"
                    )
                })
        })
        .collect()
}

/// Add the `i686-pc-windows-msvc` standard library through rustup when it is
/// missing, the way a fresh CI runner has it.
fn ensure_target_installed() -> Result<()> {
    let installed = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match installed {
        Ok(output) if output.status.success() => {
            if String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == X86_TARGET)
            {
                return Ok(());
            }
        }
        // No rustup: the toolchain manager is someone else's, and Cargo
        // reports a missing standard library itself.
        _ => return Ok(()),
    }
    eprintln!("[xtask] adding the {X86_TARGET} target for the 32-bit plug-in bridge");
    let status = Command::new("rustup")
        .args(["target", "add", X86_TARGET])
        .status()
        .context("failed to run `rustup target add`")?;
    if !status.success() {
        bail!(
            "`rustup target add {X86_TARGET}` failed with {status}; the 32-bit plug-in bridge \
             cannot be built without it"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_64_bit_windows_packages_carry_the_bridge() {
        assert!(applies_to("x86_64-pc-windows-msvc"));
        for other in [
            "aarch64-pc-windows-msvc",
            "i686-pc-windows-msvc",
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-darwin",
        ] {
            assert!(!applies_to(other), "{other}");
        }
    }

    #[test]
    fn the_bridge_is_staged_under_its_32_bit_names() {
        let staged: Vec<&str> = X86_SIDECARS.iter().map(|(_, staged)| *staged).collect();
        assert_eq!(
            staged,
            [
                "FutureboardPluginHostX86.exe",
                "FutureboardPluginScannerX86.exe"
            ]
        );
        // The Cargo binaries are the 64-bit sidecars' own.
        for (bin, _) in X86_SIDECARS {
            assert!(crate::cargo_build::SIDECAR_BINARIES.contains(bin), "{bin}");
        }
    }
}
