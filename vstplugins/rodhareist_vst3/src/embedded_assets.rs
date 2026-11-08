//! The plug-in's GPUI asset source, at the path Studio's asset modules name
//! it by. The Rodhareist editor draws its own icons as paths and embeds its
//! fonts; this serves Studio's Futureboard icon set and the check mark, which
//! the shared controls draw from SVG.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

use crate::assets;

pub struct EmbeddedAssets;

impl EmbeddedAssets {
    pub fn new() -> Self {
        Self
    }
}

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == assets::ICON_CHECK_PATH {
            return Ok(Some(Cow::Borrowed(assets::icons::CHECK.as_bytes())));
        }
        Ok(assets::futureboard_icon(path).map(|svg| Cow::Borrowed(svg.as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(assets::FUTUREBOARD_ICONS
            .iter()
            .map(|(icon, _)| *icon)
            .chain([assets::ICON_CHECK_PATH])
            .filter(|icon| icon.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
