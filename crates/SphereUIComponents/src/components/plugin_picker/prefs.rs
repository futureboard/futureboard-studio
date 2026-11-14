//! Favorites, recents, and picker UI preferences persisted beside the plugin DB.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const MAX_RECENT: usize = 32;
const PREFS_FILE: &str = "plugin_picker_prefs.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginPickerPrefs {
    pub favorites: HashSet<String>,
    pub recent: Vec<String>,
    pub window_width: f32,
    pub window_height: f32,
    pub show_details: bool,
    /// Grouping of the Add Track instrument picker: "vendor", "name" or
    /// "format". Empty (older files) means vendor.
    pub instrument_grouping: String,
}

impl PluginPickerPrefs {
    pub fn load() -> Self {
        let path = prefs_path();
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default_with_size();
        };
        serde_json::from_str(&raw).unwrap_or_else(|_| Self::default_with_size())
    }

    pub fn save(&self) -> Result<(), String> {
        let path = prefs_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, json).map_err(|e| e.to_string())
    }

    pub fn default_with_size() -> Self {
        Self {
            favorites: HashSet::new(),
            recent: Vec::new(),
            window_width: 860.0,
            window_height: 560.0,
            show_details: true,
            instrument_grouping: String::new(),
        }
    }

    pub fn is_favorite(&self, plugin_id: &str) -> bool {
        self.favorites.contains(plugin_id)
    }

    pub fn toggle_favorite(&mut self, plugin_id: &str) -> bool {
        if self.favorites.remove(plugin_id) {
            let _ = self.save();
            return false;
        }
        self.favorites.insert(plugin_id.to_string());
        let _ = self.save();
        true
    }

    pub fn record_recent(&mut self, plugin_id: &str) {
        self.recent.retain(|id| id != plugin_id);
        self.recent.insert(0, plugin_id.to_string());
        self.recent.truncate(MAX_RECENT);
        let _ = self.save();
    }
}

fn prefs_path() -> PathBuf {
    SpherePluginHost::database_dir().join(PREFS_FILE)
}
