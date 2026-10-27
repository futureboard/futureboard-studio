//! The knob cell from Studio's QuickSampler panel, which the WhiteSharp
//! panel borrows. The rest of that file needs the sampler, so the cell is
//! repeated here; keep it in step with Studio's.

use gpui::{div, px, AnyElement, IntoElement, ParentElement, Styled};

use crate::theme::{space, typography, Colors};

const KNOB_CELL_W: f32 = 58.0;

pub(crate) fn knob_cell(
    caption: &'static str,
    readout: String,
    control: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(KNOB_CELL_W))
        .gap(px(space::HAIR))
        .child(control)
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(caption),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(readout),
        )
        .into_any_element()
}
