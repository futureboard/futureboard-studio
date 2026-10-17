//! Package orchestration: build → collect → stage → validate → publish.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use cargo_metadata::MetadataCommand;

use crate::cargo_build::{self, APP_PACKAGE};
use crate::cef;
use crate::metadata::{BuildInfo, MetadataInputs};
use crate::platform::{Edition, host_target, platform_folder};
use crate::plugins;
use crate::staging::{self, BIN_DIR, StagingPlan};
use crate::validation::{self, ValidationInputs};

/// Which Built-in Plugins to build and stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSelection {
    /// Do not build any plugins (default).
    None,
    /// Build every discovered Built-in Plugin.
    All,
    /// Build only the named plugin crates (e.g. `rodharerist`, `equz8`).
    Only(Vec<String>),
}

impl PluginSelection {
    /// Resolve the CLI inputs: `--plugin <spec>` (`all` / `none` / comma list),
    /// or the legacy `--plugins` bool (= `all`). `--plugin` wins when both given.
    pub fn parse(plugin: Option<&str>, plugins_flag: bool) -> Self {
        if let Some(spec) = plugin {
            return match spec.trim().to_ascii_lowercase().as_str() {
                "all" | "*" => Self::All,
                "" | "none" => Self::None,
                _ => {
                    let names: Vec<String> = spec
                        .split(',')
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .collect();
                    if names.is_empty() {
                        Self::None
                    } else {
                        Self::Only(names)
                    }
                }
            };
        }
        if plugins_flag { Self::All } else { Self::None }
    }

    fn is_enabled(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Everything the `package` subcommand needs.
pub struct PackageOptions {
    pub profile: String,
    pub target: Option<String>,
    pub edition: Edition,
    /// Root of the distributable tree (default `out`).
    pub out_root: PathBuf,
    /// Also stage debug symbols into `symbols/`.
    pub symbols: bool,
    /// Which Built-in Plugin dynamic libraries to build and stage.
    pub plugins: PluginSelection,
    /// Stage the shared CEF runtime flat beside the binary when available.
    pub stage_cef: bool,
    /// After a macOS package, wrap the staged tree into `Futureboard Studio.app`.
    pub bundle_macos: bool,
}

/// Run the full package pipeline and return the published directory.
pub fn run(options: &PackageOptions) -> Result<PathBuf> {
    // Resolve the effective target triple (explicit flag or detected host) for
    // the build environment, platform naming, and metadata.
    let target_triple = match &options.target {
        Some(target) => target.clone(),
        None => host_target().context("could not determine host target triple")?,
    };
    let workspace = workspace_root();
    let discovered = if options.plugins.is_enabled() {
        plugin_packages()?
    } else {
        Vec::new()
    };
    // Resolve plugin selection before building the app: the native build can
    // compile plugin crates, whose build.rs must see frontend dist assets.
    let selected: Vec<&DiscoveredPlugin> = match &options.plugins {
        PluginSelection::None => Vec::new(),
        PluginSelection::All => discovered.iter().collect(),
        PluginSelection::Only(names) => {
            for requested in names {
                if !discovered.iter().any(|plugin| &plugin.name == requested) {
                    eprintln!(
                        "[xtask] warning: requested plugin `{requested}` is not a \
                         Built-in Plugin cdylib crate - skipping"
                    );
                }
            }
            discovered
                .iter()
                .filter(|plugin| names.iter().any(|name| name == &plugin.name))
                .collect()
        }
    };
    let editor_directory_names = selected_editor_directory_names(&selected);
    if !editor_directory_names.is_empty() {
        // scripts/build.ts discovers editors by their directory names. Those
        // names are not required to match Cargo package names (for example,
        // `67Clipper` contains the `clipper67` package).
        eprintln!("[xtask] installing and building editor frontends for selected plugins");
        plugins::build_editor_uis(&workspace, &editor_directory_names)?;
    }

    let cef_dist = cef::prepare_cef_dist(&workspace, &target_triple)
        .context("failed to prepare the CEF distribution")?;
    if options.stage_cef && cef_dist.is_none() {
        let checked = cef::cef_dist_candidates(&workspace, &target_triple)
            .into_iter()
            .map(|path| format!("  - {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "CEF distribution not found or invalid for {target_triple}.\n\
             Checked:\n{checked}\n\
             Install it with `cargo run -p SphereWebView --example install_cef \
             --features installer`, or pass --no-cef for an intentional \
             CEF-free developer package"
        );
    }

    // 1. Build and discover the real executable paths (app + runtime sidecars).
    // Pass the target-matched SDK explicitly so a stale shell CEF_PATH cannot
    // select a distribution for another operating system or architecture.
    let build = cargo_build::build(
        &options.profile,
        options.target.as_deref(),
        options.edition,
        cef_dist.as_deref(),
    )?;
    let executable = &build.app_executable;
    eprintln!("[xtask] built executable: {}", executable.display());

    let platform = platform_folder(&target_triple);

    // 2. Prepare staging (clean any stale directory first).
    let plan = StagingPlan::new(
        &options.out_root,
        &options.profile,
        options.edition,
        &platform,
    );
    plan.prepare()?;

    // 3. Copy required runtime files into staging — the app binary, the sidecar
    //    executables it spawns from its own directory, and any runtime libraries
    //    the build placed beside the binary.
    let binary_name = staging::stage_executable(&plan.staging_dir, executable)?;
    let engine_library_name = staging::stage_executable(&plan.staging_dir, &build.engine_library)?;
    eprintln!("[xtask] staged engine library: {engine_library_name}");

    let mut sidecar_names = Vec::with_capacity(build.sidecar_executables.len());
    for sidecar in &build.sidecar_executables {
        let name = staging::stage_executable(&plan.staging_dir, sidecar)?;
        eprintln!("[xtask] staged sidecar executable: {name}");
        sidecar_names.push(name);
    }
    if let Some(helper) = &build.cef_helper_executable {
        let name = staging::stage_executable(&plan.staging_dir, helper)?;
        eprintln!("[xtask] staged macOS CEF helper executable: {name}");
        sidecar_names.push(name);
    }

    let mut apak_tool_names = Vec::with_capacity(build.apak_executables.len());
    for tool in &build.apak_executables {
        let relative = staging::stage_executable_into(&plan.staging_dir, BIN_DIR, tool)?;
        eprintln!("[xtask] staged APAK tool: {relative}");
        apak_tool_names.push(relative);
    }

    let siblings = staging::stage_runtime_siblings(&plan.staging_dir, executable)?;
    for sibling in &siblings {
        eprintln!("[xtask] staged runtime library: {sibling}");
    }
    if let Some(handler_name) = staging::crashpad_handler_name(&target_triple) {
        let crashpad_handler =
            staging::stage_crashpad_handler(&plan.staging_dir, executable, &target_triple)?;
        debug_assert_eq!(crashpad_handler, handler_name);
        eprintln!("[xtask] staged Crashpad handler: {crashpad_handler}");
    } else {
        eprintln!("[xtask] Crashpad external handler not staged for target `{target_triple}`");
    }

    // 4. Create expected directories.
    staging::create_layout_dirs(&plan.staging_dir)?;

    // 4a. Stage the shared CEF runtime flat beside the binary (never a CEF/
    //     subdir). A normal package is not publishable without it; developers
    //     who intentionally need a CEF-free tree must opt out with --no-cef.
    let mut cef_staged = false;
    if options.stage_cef {
        let dist = cef_dist
            .as_ref()
            .expect("stage_cef preflight requires a distribution");
        let report = cef::stage_cef(&plan.staging_dir, dist, &target_triple)
            .context("failed to stage the CEF runtime")?;
        eprintln!(
            "[xtask] staged CEF runtime flat: {} files + {} locales",
            report.runtime_files.len(),
            report.locale_count
        );
        cef_staged = true;
    }

    // 4b. Optionally build and stage Built-in Plugin dynamic libraries.
    if options.plugins.is_enabled() {
        // A plugin that embeds its editor is not release-ready when the static
        // bundle is absent: compiling it would silently embed an empty asset
        // table. A plugin that merely *ships* a bundle nothing consumes yet is
        // unaffected, so it must not block a release.
        for plugin in &selected {
            if plugins::embeds_editor_ui(&plugin.crate_dir)
                && !plugins::editor_ui_built(&plugin.crate_dir)
            {
                bail!(
                    "plugin `{}` embeds its editor but has no built dist/index.html; \
                     run its editor build before packaging",
                    plugin.name,
                );
            }
        }
        let packages: Vec<String> = selected.iter().map(|p| p.name.clone()).collect();
        let built = plugins::build_plugins(
            &packages,
            &options.profile,
            options.target.as_deref(),
            options.edition,
        )?;
        let staged = plugins::stage_plugins(&plan.staging_dir, &built, &target_triple)?;
        if staged.is_empty() {
            eprintln!("[xtask] plugin build requested but no plugin cdylibs were produced");
        }
        for name in &staged {
            eprintln!("[xtask] staged plugin: Plugins/{name}");
        }
        // Completeness check only when building the full set (a subset omits the
        // rest on purpose).
        if options.plugins == PluginSelection::All {
            let missing = plugins::missing_builtin_plugins(&staged, &target_triple);
            if !missing.is_empty() {
                bail!(
                    "expected built-in plugin(s) were not staged: {}",
                    missing.join(", "),
                );
            }
        }
    }

    // 5. Optional symbols.
    if options.symbols {
        // The engine library holds nearly all of the Studio's code, so its
        // symbols matter more than the launcher's.
        let mut staged = staging::stage_symbols(&plan.staging_dir, executable)?;
        staged.extend(staging::stage_symbols(
            &plan.staging_dir,
            &build.engine_library,
        )?);
        if staged.is_empty() {
            eprintln!("[xtask] --symbols requested but no .pdb was found beside the binary");
        }
        for path in &staged {
            eprintln!("[xtask] staged symbols: {path}");
        }
    }

    // 6. Write build metadata.
    let version = package_version()?;
    let info = BuildInfo::collect(&MetadataInputs {
        binary: &binary_name,
        sidecars: &sidecar_names,
        edition: options.edition,
        profile: &options.profile,
        target: &target_triple,
        platform: &platform,
        version: &version,
    });
    staging::write_build_info(&plan.staging_dir, &info.to_json()?)?;

    // 7. Validate the staged application before publishing.
    validation::validate_staging(&ValidationInputs {
        staging_dir: &plan.staging_dir,
        binary_name: &binary_name,
        libraries: std::slice::from_ref(&engine_library_name),
        sidecars: &sidecar_names,
        tools: &apak_tool_names,
        symbols_enabled: options.symbols,
        triple: &target_triple,
        cef_staged,
    })
    .context("staged package failed validation; previous output left untouched")?;

    // 8. Atomically publish, then tidy the (now empty) staging root.
    staging::publish(&plan.staging_dir, &plan.final_dir)?;
    staging::cleanup_staging_root_if_empty(&plan.staging_dir);
    eprintln!("[xtask] published package: {}", plan.final_dir.display());

    if options.bundle_macos {
        if let Some(app_dir) = bundle_macos_app(&workspace, &plan.final_dir, &target_triple)? {
            eprintln!("[xtask] bundled macOS app: {}", app_dir.display());
        }
    }

    Ok(plan.final_dir)
}

/// Sibling of the staged runtime so `bundle-macos.sh` can `cp -a PACKAGE_DIR/.`
/// into `Contents/MacOS` without copying the `.app` into itself.
fn macos_app_bundle_out(package_dir: &Path) -> PathBuf {
    let platform = package_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("macos");
    package_dir
        .parent()
        .map(|parent| parent.join(format!("{platform}-app")))
        .unwrap_or_else(|| package_dir.join("app"))
}

/// Wrap a published macOS runtime tree in a real `Contents/` `.app` bundle.
fn bundle_macos_app(
    workspace: &Path,
    package_dir: &Path,
    target_triple: &str,
) -> Result<Option<PathBuf>> {
    if !target_triple.ends_with("apple-darwin") {
        return Ok(None);
    }
    if !cfg!(target_os = "macos") {
        eprintln!(
            "[xtask] skipping macOS .app bundle: host is not macOS (need PlistBuddy/codesign)"
        );
        return Ok(None);
    }
    if !package_dir.join("FutureboardNative").is_file() {
        return Ok(None);
    }

    let script = workspace.join("packaging/native/bundle-macos.sh");
    if !script.is_file() {
        bail!("macOS bundle script missing: {}", script.display());
    }
    let app_out = macos_app_bundle_out(package_dir);
    let status = Command::new("bash")
        .arg(&script)
        .arg(package_dir)
        .arg(&app_out)
        .status()
        .with_context(|| format!("failed to run {}", script.display()))?;
    if !status.success() {
        bail!(
            "macOS bundle script failed with status {status} ({})",
            script.display()
        );
    }
    let app_dir = app_out.join("Futureboard Studio.app");
    let contents = app_dir.join("Contents");
    if !contents.join("MacOS/FutureboardNative").is_file() {
        bail!(
            "macOS bundle is missing Contents/MacOS/FutureboardNative: {}",
            app_dir.display()
        );
    }
    if app_dir.join("Content").exists() && !contents.exists() {
        bail!(
            "macOS bundle used Content/ instead of Contents/: {}",
            app_dir.display()
        );
    }
    Ok(Some(app_dir))
}

/// The Futureboard workspace root (xtask lives at `<root>/xtask`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live under the workspace root")
        .to_path_buf()
}

/// A discovered Built-in Plugin package (name + crate directory).
struct DiscoveredPlugin {
    name: String,
    crate_dir: PathBuf,
}

fn selected_editor_directory_names(selected: &[&DiscoveredPlugin]) -> Vec<String> {
    selected
        .iter()
        .filter(|plugin| plugins::has_editor_ui(&plugin.crate_dir))
        .filter_map(|plugin| plugin.crate_dir.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

/// Workspace member packages that build a Built-in Plugin dynamic library
/// (`cdylib`/`dylib`) and live under `crates/BuiltinAudioPlugins/crates`.
fn plugin_packages() -> Result<Vec<DiscoveredPlugin>> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("failed to query cargo metadata for plugin discovery")?;
    // Normalize to forward slashes so the directory match is OS-independent.
    let marker = plugins::PLUGIN_CRATES_DIR; // "crates/BuiltinAudioPlugins/crates"
    let mut packages = Vec::new();
    for package in &metadata.packages {
        let manifest = package.manifest_path.as_std_path();
        let in_plugin_dir = manifest
            .to_string_lossy()
            .replace('\\', "/")
            .contains(marker);
        let produces_dylib = package.targets.iter().any(|target| {
            target
                .kind
                .iter()
                .any(|kind| matches!(kind.as_str(), "cdylib" | "dylib"))
        });
        if in_plugin_dir && produces_dylib {
            let crate_dir = manifest
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            packages.push(DiscoveredPlugin {
                name: package.name.to_string(),
                crate_dir,
            });
        }
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(packages)
}

/// Read the `futureboard_native` package version from workspace metadata,
/// reusing the existing application versioning source of truth.
fn package_version() -> Result<String> {
    let metadata = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("failed to query cargo metadata for the application version")?;
    metadata
        .packages
        .iter()
        .find(|package| package.name.as_str() == APP_PACKAGE)
        .map(|package| package.version.to_string())
        .ok_or_else(|| anyhow!("package `{APP_PACKAGE}` not found in workspace metadata"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{
        DiscoveredPlugin, PluginSelection, macos_app_bundle_out, selected_editor_directory_names,
    };

    #[test]
    fn plugin_selection_all_and_none() {
        assert_eq!(
            PluginSelection::parse(Some("all"), false),
            PluginSelection::All
        );
        assert_eq!(
            PluginSelection::parse(Some("ALL"), false),
            PluginSelection::All
        );
        assert_eq!(
            PluginSelection::parse(Some("*"), false),
            PluginSelection::All
        );
        assert_eq!(
            PluginSelection::parse(Some("none"), false),
            PluginSelection::None
        );
        assert_eq!(
            PluginSelection::parse(Some(""), true),
            PluginSelection::None
        );
    }

    #[test]
    fn plugin_selection_list_is_trimmed() {
        assert_eq!(
            PluginSelection::parse(Some("rodharerist, equz8 ,,fa76"), false),
            PluginSelection::Only(vec![
                "rodharerist".to_string(),
                "equz8".to_string(),
                "fa76".to_string(),
            ])
        );
    }

    #[test]
    fn legacy_plugins_flag_means_all_and_default_is_none() {
        assert_eq!(PluginSelection::parse(None, true), PluginSelection::All);
        assert_eq!(PluginSelection::parse(None, false), PluginSelection::None);
        // Explicit --plugin wins over the legacy bool.
        assert_eq!(
            PluginSelection::parse(Some("none"), true),
            PluginSelection::None
        );
    }

    #[test]
    fn editor_build_uses_directory_name_instead_of_cargo_package_name() {
        let temp = tempfile::tempdir().unwrap();
        let crate_dir = temp.path().join("67Clipper");
        fs::create_dir_all(crate_dir.join("editorui")).unwrap();
        fs::write(crate_dir.join("editorui/package.json"), "{}").unwrap();
        let clipper = DiscoveredPlugin {
            name: "clipper67".to_string(),
            crate_dir,
        };

        assert_eq!(
            selected_editor_directory_names(&[&clipper]),
            vec!["67Clipper".to_string()]
        );
    }

    #[test]
    fn macos_app_bundle_is_a_sibling_not_nested_in_the_package() {
        let package = PathBuf::from("/out/release/community/macos-arm64");
        let app_out = macos_app_bundle_out(&package);
        assert_eq!(
            app_out,
            PathBuf::from("/out/release/community/macos-arm64-app")
        );
        assert!(
            !app_out.starts_with(&package),
            "bundle output must not live inside the staged runtime tree"
        );
    }
}
