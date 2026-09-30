//! The Preferences window's page vocabulary.
//!
//! A page is a stack of groups. A group is a quiet caption over one inset
//! plate; the plate holds rows separated by hairlines. A row is its label —
//! with at most one line saying what it does — on the left and its control on
//! the right, so every control on a page lines up on one edge and every label
//! reads down the other.
//!
//! Pages are built as data ([`PrefGroup`] / [`PrefRow`]) rather than straight
//! into elements, so search can show the matching rows of every page with the
//! same code that draws one page.

use super::*;

pub(crate) use crate::components::settings_layout::{
    PREFS_CONTENT_PAD_X, PREFS_CONTROL_W, PREFS_ROW_PAD_X,
};

/// Minimum row height: a 24 px control with air above and below.
const ROW_MIN_H: f32 = 40.0;

/// One row of a group.
pub(crate) enum PrefRow {
    /// Label (and optional one-line description) with a control on the right.
    Field {
        label: String,
        description: Option<String>,
        keywords: &'static [&'static str],
        control: gpui::AnyElement,
    },
    /// A full-width element — a device list, a report. `label` is what search
    /// matches it by.
    Block {
        label: String,
        keywords: &'static [&'static str],
        element: gpui::AnyElement,
    },
}

impl PrefRow {
    pub(crate) fn field(
        label: impl Into<String>,
        keywords: &'static [&'static str],
        control: impl IntoElement,
    ) -> Self {
        Self::Field {
            label: label.into(),
            description: None,
            keywords,
            control: control.into_any_element(),
        }
    }

    pub(crate) fn described(
        label: impl Into<String>,
        description: impl Into<String>,
        keywords: &'static [&'static str],
        control: impl IntoElement,
    ) -> Self {
        Self::Field {
            label: label.into(),
            description: Some(description.into()),
            keywords,
            control: control.into_any_element(),
        }
    }

    pub(crate) fn block(
        label: impl Into<String>,
        keywords: &'static [&'static str],
        element: impl IntoElement,
    ) -> Self {
        Self::Block {
            label: label.into(),
            keywords,
            element: element.into_any_element(),
        }
    }

    fn matches(&self, query: &str) -> bool {
        let (label, description, keywords) = match self {
            Self::Field {
                label,
                description,
                keywords,
                ..
            } => (label.as_str(), description.as_deref(), *keywords),
            Self::Block {
                label, keywords, ..
            } => (label.as_str(), None, *keywords),
        };
        label.to_lowercase().contains(query)
            || description.is_some_and(|d| d.to_lowercase().contains(query))
            || keywords.iter().any(|k| k.contains(query))
    }
}

/// A captioned group of rows, with an optional note under the plate.
pub(crate) struct PrefGroup {
    pub title: String,
    pub rows: Vec<PrefRow>,
    pub note: Option<String>,
}

impl PrefGroup {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            rows: Vec::new(),
            note: None,
        }
    }

    pub(crate) fn row(mut self, row: PrefRow) -> Self {
        self.rows.push(row);
        self
    }

    pub(crate) fn rows(mut self, rows: impl IntoIterator<Item = PrefRow>) -> Self {
        self.rows.extend(rows);
        self
    }

    pub(crate) fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// The group narrowed to the rows matching `query` (already lower-case).
    /// A match on the group's own title keeps every row.
    pub(crate) fn filtered(self, query: &str) -> Option<Self> {
        if self.title.to_lowercase().contains(query) {
            return Some(self);
        }
        let rows: Vec<PrefRow> = self
            .rows
            .into_iter()
            .filter(|row| row.matches(query))
            .collect();
        (!rows.is_empty()).then_some(Self {
            title: self.title,
            rows,
            note: None,
        })
    }

    pub(crate) fn render(self) -> gpui::AnyElement {
        use crate::theme::{radius, space, typography};
        let count = self.rows.len();
        let mut plate = div()
            .flex()
            .flex_col()
            .rounded(px(radius::SURFACE))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_card())
            .overflow_hidden();
        for (index, row) in self.rows.into_iter().enumerate() {
            plate = plate.child(render_row(row));
            if index + 1 < count {
                plate = plate.child(
                    div()
                        .h(px(1.0))
                        .ml(px(PREFS_ROW_PAD_X))
                        .bg(Colors::divider()),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .gap(px(space::SNUG))
            .child(
                div()
                    .px(px(space::TIGHT))
                    .text_size(px(typography::DENSE_LABEL))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_muted())
                    .child(self.title),
            )
            .child(plate)
            .children(self.note.map(|note| {
                div()
                    .px(px(space::TIGHT))
                    .text_size(px(typography::DENSE_LABEL))
                    .line_height(px(15.0))
                    .text_color(Colors::text_faint())
                    .child(note)
            }))
            .into_any_element()
    }
}

fn render_row(row: PrefRow) -> gpui::AnyElement {
    use crate::theme::{space, typography};
    match row {
        PrefRow::Field {
            label,
            description,
            control,
            ..
        } => div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .min_h(px(ROW_MIN_H))
            .px(px(PREFS_ROW_PAD_X))
            .py(px(space::SNUG))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(space::HAIR))
                    .child(
                        div()
                            .text_size(px(typography::UI_SM))
                            .text_color(Colors::text_primary())
                            .child(label),
                    )
                    .children(description.map(|description| {
                        div()
                            .text_size(px(typography::DENSE_LABEL))
                            .line_height(px(14.0))
                            .text_color(Colors::text_muted())
                            .child(description)
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .justify_end()
                    .items_center()
                    .child(control),
            )
            .into_any_element(),
        PrefRow::Block { element, .. } => div()
            .px(px(PREFS_ROW_PAD_X))
            .py(px(space::BASE))
            .child(element)
            .into_any_element(),
    }
}

/// A dropdown at the page's one control width.
pub(crate) fn pref_select(
    combo: HardwareCombo,
    trigger_id: &'static str,
    selected: &str,
    open_combo: Option<HardwareCombo>,
    on_toggle: Arc<dyn Fn(HardwareCombo, Option<OverlayAnchor>, &mut Window, &mut App) + 'static>,
) -> gpui::AnyElement {
    div()
        .w(px(PREFS_CONTROL_W))
        .child(hardware_select(
            combo, trigger_id, selected, open_combo, on_toggle,
        ))
        .into_any_element()
}

/// An on/off switch for a boolean setting. The row's label names it.
pub(crate) fn pref_switch(
    id: impl Into<gpui::ElementId>,
    on: bool,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    box_list_toggle(id, on, on_toggle).into_any_element()
}

/// A slider with its value beside it.
pub(crate) fn pref_slider(
    id: &'static str,
    fraction: f32,
    value: String,
    on_change: impl Fn(&f32, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    use crate::theme::{space, typography};
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .w(px(PREFS_CONTROL_W))
        .child(div().flex_1().min_w_0().child(slider(
            id,
            fraction.clamp(0.0, 1.0),
            Colors::accent_primary(),
            on_change,
        )))
        .child(
            div()
                .w(px(56.0))
                .flex_shrink_0()
                .text_align(gpui::TextAlign::Right)
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(value),
        )
        .into_any_element()
}

/// A value with − / + beside it.
pub(crate) fn pref_stepper(
    id: &'static str,
    value: String,
    on_down: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    on_up: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    use crate::theme::{radius, size, space, typography};
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::TIGHT))
        .child(fb_stepper_button((id, 0usize), "−", on_down))
        .child(
            div()
                .min_w(px(72.0))
                .h(px(size::COMFORTABLE))
                .px(px(space::BASE))
                .rounded(px(radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_input())
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_primary())
                .child(value),
        )
        .child(fb_stepper_button((id, 1usize), "+", on_up))
        .into_any_element()
}

/// A read-only value.
pub(crate) fn pref_value(text: impl Into<String>) -> gpui::AnyElement {
    div()
        .text_size(px(crate::theme::typography::UI_XS))
        .text_color(Colors::text_secondary())
        .child(text.into())
        .into_any_element()
}

/// A button that opens another window or runs a command.
pub(crate) fn pref_button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    fb_button(id, label, FbButtonKind::Default, enabled, on_click).into_any_element()
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn group() -> PrefGroup {
        PrefGroup::new("Transport")
            .row(PrefRow::field("Spacebar", &["play", "stop"], div()))
            .row(PrefRow::described(
                "Return to start",
                "Moves the playhead back on stop.",
                &[],
                div(),
            ))
            .note("A note")
    }

    fn labels(group: &PrefGroup) -> Vec<&str> {
        group
            .rows
            .iter()
            .map(|row| match row {
                PrefRow::Field { label, .. } | PrefRow::Block { label, .. } => label.as_str(),
            })
            .collect()
    }

    #[test]
    fn search_keeps_the_rows_that_match_by_label_keyword_or_description() {
        assert_eq!(labels(&group().filtered("space").unwrap()), ["Spacebar"]);
        assert_eq!(labels(&group().filtered("stop").unwrap()).len(), 2);
        assert_eq!(
            labels(&group().filtered("playhead").unwrap()),
            ["Return to start"]
        );
        assert!(group().filtered("midi").is_none());
    }

    #[test]
    fn a_match_on_the_group_keeps_all_of_it() {
        let group = group().filtered("transport").unwrap();
        assert_eq!(labels(&group).len(), 2);
        assert!(group.note.is_some());
    }
}
