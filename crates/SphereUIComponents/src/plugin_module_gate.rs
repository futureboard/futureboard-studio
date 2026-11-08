//! Which plug-in module files Studio may load.
//!
//! A project (`.fbproj`) or a preset (`.pst`) names each insert's plug-in by
//! its module path. Loading whatever a shared file names would let that file
//! run a native module from anywhere — a download folder, a network share —
//! with the user's rights. So a module is loaded only when this machine's
//! plug-in scan put that exact path in the catalog, or when the user picked
//! the file by hand in a Locate dialog this session. Anything else is refused
//! and the insert is reported missing, which the Missing Media window offers
//! to relink to where the scan found the plug-in.
//!
//! Paths are compared as text (case-folded on Windows), never resolved
//! through the file system: a path that only *leads* to a catalog module
//! (`..`, links) is a different path and is refused.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::components::timeline::timeline_state::InsertPluginFormat;

#[derive(Default)]
struct ModuleSet {
    /// The catalog's module paths; `None` until a catalog has been read.
    catalog: Option<HashSet<String>>,
    /// Files the user picked by hand this session.
    chosen: HashSet<String>,
}

impl ModuleSet {
    fn verdict(&self, path: &Path) -> Result<(), String> {
        let key = module_key(path);
        if self.chosen.contains(&key) {
            return Ok(());
        }
        match &self.catalog {
            Some(catalog) if catalog.contains(&key) => Ok(()),
            Some(_) => Err(format!(
                "Plug-in not in this computer's plug-in catalog: {}. Scan plug-ins, or relink \
                 it to the installed copy.",
                path.display()
            )),
            None => Err(format!(
                "Plug-in catalog unavailable, so {} was not loaded. Scan plug-ins, then reopen.",
                path.display()
            )),
        }
    }
}

fn gate() -> &'static Mutex<ModuleSet> {
    static GATE: OnceLock<Mutex<ModuleSet>> = OnceLock::new();
    GATE.get_or_init(Mutex::default)
}

/// The text a path is compared by.
fn module_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        text.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    #[cfg(not(windows))]
    {
        let trimmed = text.trim_end_matches('/');
        if trimmed.is_empty() {
            text.into_owned()
        } else {
            trimmed.to_string()
        }
    }
}

fn catalog_keys<'a>(paths: impl IntoIterator<Item = &'a Path>) -> HashSet<String> {
    paths
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .map(module_key)
        .collect()
}

/// A catalog was (re)read: from now on its modules are the ones that load.
pub fn remember_catalog<'a>(paths: impl IntoIterator<Item = &'a Path>) {
    let keys = catalog_keys(paths);
    if let Ok(mut gate) = gate().lock() {
        gate.catalog = Some(keys);
    }
}

/// The user picked this module file by hand: trust it for this session.
pub fn trust_user_choice(path: &Path) {
    if let Ok(mut gate) = gate().lock() {
        gate.chosen.insert(module_key(path));
    }
}

/// Whether the module at `path` may be loaded; `Err` is the reason to show on
/// the insert. Reads the scan database once when no catalog has been handed
/// in yet — a project restores its plug-ins before the Studio's own catalog
/// load has finished.
pub fn check_module(path: &Path) -> Result<(), String> {
    let mut gate = gate().lock().unwrap_or_else(|poison| poison.into_inner());
    if gate.catalog.is_none() {
        use SpherePluginHost::CatalogLoad;
        match SpherePluginHost::PluginRegistry::load_catalog() {
            CatalogLoad::Loaded { catalog, .. } => {
                gate.catalog = Some(catalog_keys(
                    catalog.plugins.iter().map(|entry| entry.path.as_path()),
                ));
            }
            // Nothing scanned yet: nothing external loads until a scan, whose
            // catalog reload hands the result in.
            CatalogLoad::MissingDatabase { .. } => gate.catalog = Some(HashSet::new()),
            // Unreadable now: refuse, and read it again next time.
            CatalogLoad::Error { .. } => {}
        }
    }
    gate.verdict(path)
}

/// Why an insert's plug-in module must not load, or `None` when it may (or
/// has no module file: a built-in, an Audio Unit). Covers a module that is not
/// where the slot says as well as one the catalog does not know.
pub fn module_refusal(
    is_builtin: bool,
    format: Option<InsertPluginFormat>,
    path: Option<&Path>,
) -> Option<String> {
    if is_builtin || format.is_some_and(|format| !format.has_module_file()) {
        return None;
    }
    let Some(path) = path.filter(|path| !path.as_os_str().is_empty()) else {
        return Some("Plugin file not found".to_string());
    };
    if !path.exists() {
        return Some(format!("Plugin file not found: {}", path.display()));
    }
    check_module(path).err()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(catalog: &[&str]) -> ModuleSet {
        ModuleSet {
            catalog: Some(catalog_keys(catalog.iter().map(Path::new))),
            chosen: HashSet::new(),
        }
    }

    #[test]
    fn a_module_the_catalog_does_not_know_is_refused() {
        let gate = set(&["/plugins/Good.vst3"]);
        assert!(gate.verdict(Path::new("/plugins/Good.vst3")).is_ok());
        assert!(gate.verdict(Path::new("/plugins/Good.vst3/")).is_ok());
        let refused = gate.verdict(Path::new("/downloads/Evil.vst3")).unwrap_err();
        assert!(refused.contains("not in this computer's plug-in catalog"));
        // A path that only leads to a catalog module is not that module.
        assert!(gate
            .verdict(Path::new("/downloads/../plugins/Good.vst3"))
            .is_err());
        assert!(gate
            .verdict(Path::new("//server/share/plugins/Good.vst3"))
            .is_err());
    }

    #[test]
    fn no_catalog_refuses_everything_until_one_is_read() {
        let gate = ModuleSet::default();
        assert!(gate.verdict(Path::new("/plugins/Good.vst3")).is_err());
    }

    #[test]
    fn a_file_the_user_picked_by_hand_loads() {
        let mut gate = set(&[]);
        let picked = Path::new("/elsewhere/Mine.clap");
        assert!(gate.verdict(picked).is_err());
        gate.chosen.insert(module_key(picked));
        assert!(gate.verdict(picked).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_without_case_or_separator_spelling() {
        let gate = set(&[r"C:\Program Files\Common Files\VST3\Good.vst3"]);
        assert!(gate
            .verdict(Path::new("c:/program files/common files/vst3/good.VST3"))
            .is_ok());
        assert!(gate
            .verdict(Path::new(
                r"\\?\C:\Program Files\Common Files\VST3\Good.vst3"
            ))
            .is_ok());
        assert!(gate
            .verdict(Path::new(r"\\attacker\share\Good.vst3"))
            .is_err());
    }

    #[test]
    fn built_ins_and_audio_units_have_no_module_to_check() {
        assert_eq!(
            module_refusal(true, Some(InsertPluginFormat::Vst3), None),
            None
        );
        assert_eq!(
            module_refusal(false, Some(InsertPluginFormat::Au), None),
            None
        );
        assert_eq!(
            module_refusal(false, Some(InsertPluginFormat::Vst3), None).as_deref(),
            Some("Plugin file not found")
        );
    }
}
