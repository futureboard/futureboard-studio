use builtin_ui_embed::{EmbeddedPluginUi, EmbeddedUiAsset, EmbeddedUiAssetTable};

include!(concat!(env!("OUT_DIR"), "/embedded_ui_assets.rs"));

pub const UI_ORIGIN: &str = "drumsampler";

pub struct DrumSamplerUi;

impl DrumSamplerUi {
    pub fn table() -> EmbeddedUiAssetTable {
        EmbeddedUiAssetTable::new(EMBEDDED_UI_ASSETS)
    }

    pub fn is_embedded() -> bool {
        !EMBEDDED_UI_ASSETS.is_empty()
    }
}

impl EmbeddedPluginUi for DrumSamplerUi {
    fn get_ui_asset(path: &str) -> Option<EmbeddedUiAsset> {
        Self::table().get(path).copied()
    }

    fn resolve_ui_asset(path: &str) -> Option<EmbeddedUiAsset> {
        Self::table().resolve(path).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_lookup_is_total_and_rejects_traversal() {
        for path in ["", "/", "/index.html", "/../secret", "\\windows\\path"] {
            let _ = DrumSamplerUi::resolve_ui_asset(path);
        }
        assert!(DrumSamplerUi::resolve_ui_asset("/../../secret").is_none());
    }
}
