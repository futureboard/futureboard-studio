use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct HostStatus {
    pub available: bool,
    pub backend: String,
    pub vst3_sdk: bool,
    pub clap_sdk: bool,
    pub clap_helpers: bool,
    pub message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub category: String,
    pub sub_categories: Option<String>,
    pub format: String,
    pub path: String,
    pub module_path: Option<String>,
    pub class_id: Option<String>,
    pub version: Option<String>,
    pub sdk_version: Option<String>,
    pub is_shell_child: bool,
    /// The module also registers an ARA main-factory class for this plug-in, so
    /// it can be driven through ARA instead of as a plain insert.
    pub is_ara: bool,
    pub sdk_metadata_loaded: bool,
    /// Why the module could not be opened, when `sdk_metadata_loaded` is false.
    /// Carried so a failed plug-in is reportable by path *and* reason instead of
    /// silently becoming a phantom row named after its file.
    pub load_error: Option<String>,
}
