//! Pieces of the plug-in list shared outside its rows: the drag payload, the
//! scan-status label, and the loading skeleton.

use gpui::{div, px, Div, IntoElement, ParentElement, Styled};

use crate::components::plugin_picker::body::ROW_H;
use crate::theme::{radius, size, space, Colors};
use SpherePluginHost::{PluginKind, PluginScanStatus, PluginStatus, RegistryPlugin};

#[derive(Clone, Debug)]
pub struct PluginDragItem {
    pub plugin_id: String,
    pub label: String,
    pub kind: PluginKind,
}

pub fn scan_status_label(plugin: &RegistryPlugin) -> Option<&'static str> {
    match plugin.scan_status {
        PluginScanStatus::Crashed => Some("Crashed"),
        PluginScanStatus::Failed | PluginScanStatus::MetadataOnly => Some("Failed"),
        PluginScanStatus::Skipped | PluginScanStatus::Disabled => Some("Disabled"),
        PluginScanStatus::Success | PluginScanStatus::Ok => {
            if plugin.status == PluginStatus::MissingPreset {
                Some("Missing")
            } else if !plugin.supports_insert() {
                Some("Unsupported")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// A placeholder row with the shape of a real one: glyph tile, name, subtitle.
fn skeleton_row(index: usize) -> impl IntoElement {
    let alpha = 0.05 + ((index % 3) as f32) * 0.015;
    let block = move |w: f32, h: f32| {
        div()
            .h(px(h))
            .w(px(w))
            .rounded(px(radius::MICRO))
            .bg(Colors::with_alpha(Colors::text_primary(), alpha))
    };
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .h(px(ROW_H))
        .px(px(space::LOOSE))
        .child(block(size::DEFAULT, size::DEFAULT))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(space::TIGHT))
                .child(block(120.0 + (index % 4) as f32 * 24.0, 8.0))
                .child(block(96.0, 6.0)),
        )
}

pub fn skeleton_body() -> Div {
    let mut col = div().flex().flex_col().w_full();
    for i in 0..16 {
        col = col.child(skeleton_row(i));
    }
    col
}
