//! The Studio paths the theme and colour modules resolve, at the same places
//! Studio puts them.

use std::path::PathBuf;

const APP_NAME: &str = "Futureboard Studio";

pub struct FutureboardPaths {
    pub app_data: PathBuf,
    pub themes: PathBuf,
}

impl FutureboardPaths {
    pub fn resolve() -> Self {
        let app_data = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_NAME);
        let themes = app_data.join("Extensions").join("Themes");
        Self { app_data, themes }
    }
}
