use std::collections::HashSet;
use std::sync::Arc;

use gpui::{
    div, img, px, svg, App, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    StatefulInteractiveElement, Styled, StyledImage, Window,
};

use crate::assets;
use crate::theme::Colors;

pub fn combobox_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_COMBOBOX_DEBUG").is_some()
            || std::env::var_os("FUTUREBOARD_OVERLAY_DEBUG").is_some()
    })
}

fn combobox_debug(message: &str) {
    if combobox_debug_enabled() {
        eprintln!("[combobox] {message}");
    }
}

/// Remove duplicate labels while preserving first-seen order.
pub fn dedupe_preserve_order(options: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(options.len());
    for option in options {
        if seen.insert(option.clone()) {
            out.push(option.clone());
        } else if combobox_debug_enabled() {
            combobox_debug(&format!("duplicate option detected: {option}"));
        }
    }
    out
}

#[derive(Clone, Copy)]
pub struct ComboBoxOption<T: Copy + PartialEq + 'static> {
    pub label: &'static str,
    pub value: T,
}

pub fn combo_box_trigger(
    id: impl Into<gpui::ElementId>,
    label: impl Into<String>,
    open: bool,
    on_mouse_down: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .h(px(30.0))
        .w_full()
        .min_w(px(0.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(if open {
            Colors::border_focus()
        } else {
            Colors::border_subtle()
        })
        .bg(if open {
            Colors::surface_card()
        } else {
            Colors::surface_input()
        })
        .px(px(9.0))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(|s| {
            s.bg(Colors::surface_control_hover())
                .border_color(Colors::border_strong())
        })
        .on_mouse_down(gpui::MouseButton::Left, on_mouse_down)
        .child(
            div()
                .min_w(px(0.0))
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .truncate()
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_primary())
                .child(label.into()),
        )
        .child(
            svg()
                .path(assets::ICON_CHEVRON_DOWN_PATH)
                .w(px(11.0))
                .h(px(11.0))
                .flex_shrink_0()
                .text_color(Colors::text_faint()),
        )
}

pub fn combo_box_menu<T: Copy + PartialEq + 'static>(
    id: impl Into<gpui::ElementId>,
    position: crate::overlay::OverlayPosition,
    selected: T,
    options: &'static [ComboBoxOption<T>],
    on_select: Arc<dyn Fn(T, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let left: f32 = position.x.into();
    let top: f32 = position.y.into();
    let width: f32 = position.width.map(|w| w.into()).unwrap_or(120.0);
    let max_h: f32 = position.max_height.map(|h| h.into()).unwrap_or(200.0);
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .max_h(px(max_h))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_card())
        .shadow(vec![gpui::BoxShadow {
            color: Colors::surface_overlay().into(),
            offset: gpui::point(px(0.0), px(10.0)),
            blur_radius: px(28.0),
            spread_radius: px(0.0),
            inset: false,
        }])
        .p(px(4.0))
        .id(id)
        .overflow_y_scroll()
        .occlude()
        .children(options.iter().enumerate().map(|(index, option)| {
            let active = option.value == selected;
            let value = option.value;
            let on_select = on_select.clone();
            div()
                .id(("combo-box-option", index))
                .h(px(25.0))
                .w_full()
                .rounded(px(crate::theme::radius::CONTROL))
                .px(px(8.0))
                .flex()
                .items_center()
                .justify_between()
                .bg(if active {
                    Colors::accent_muted()
                } else {
                    gpui::transparent_black().into()
                })
                .text_size(px(10.5))
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_control_hover()))
                .on_click(move |_, window, cx| on_select(value, window, cx))
                .child(option.label)
                .children(active.then(|| {
                    svg()
                        .path(assets::ICON_CHECK_PATH)
                        .w(px(11.0))
                        .h(px(11.0))
                        .text_color(Colors::accent_primary())
                }))
        }))
}

/// The visual that leads a menu row.
///
/// A menu of endpoint names is a wall of text in which "Focusrite USB ASIO",
/// "Audio Jam", and a person are the same shape. The glyph is what makes the
/// *kind* of thing readable before the name is: hardware, a network send, or
/// somebody's face.
#[derive(Clone)]
pub enum MenuGlyph {
    /// No leading visual. Rows still align, so a menu may mix this with others.
    None,
    /// An asset-path SVG, tinted with the row's own text colour.
    Svg(&'static str),
    /// A downloaded picture, drawn as a circle. See
    /// [`crate::account::profile_picture`].
    Picture(Arc<gpui::Image>),
    /// A monogram, for a person whose picture is missing or has not landed yet.
    /// Never more than two characters.
    Monogram(String),
}

/// One row of a menu: what it says, and what it is.
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub glyph: MenuGlyph,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, glyph: MenuGlyph) -> Self {
        Self {
            label: label.into(),
            glyph,
        }
    }

    pub fn plain(label: impl Into<String>) -> Self {
        Self::new(label, MenuGlyph::None)
    }
}

/// Size of a menu row's leading visual, in logical units. One number for every
/// kind so the labels line up whatever the row is showing.
const GLYPH_SIZE: f32 = 14.0;

fn render_glyph(glyph: &MenuGlyph, tint: gpui::Rgba) -> gpui::AnyElement {
    let frame = div().flex_none().w(px(GLYPH_SIZE)).h(px(GLYPH_SIZE));
    match glyph {
        MenuGlyph::None => frame.into_any_element(),
        MenuGlyph::Svg(path) => frame
            .child(
                svg()
                    .path(*path)
                    .w(px(GLYPH_SIZE - 2.0))
                    .h(px(GLYPH_SIZE - 2.0))
                    .text_color(tint),
            )
            .flex()
            .items_center()
            .justify_center()
            .into_any_element(),
        MenuGlyph::Picture(image) => frame
            .rounded(px(GLYPH_SIZE / 2.0))
            .overflow_hidden()
            .child(
                img(gpui::ImageSource::Image(image.clone()))
                    .object_fit(ObjectFit::Cover)
                    .w(px(GLYPH_SIZE))
                    .h(px(GLYPH_SIZE)),
            )
            .into_any_element(),
        MenuGlyph::Monogram(text) => frame
            .rounded(px(GLYPH_SIZE / 2.0))
            .bg(Colors::surface_control_hover())
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(7.5))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_secondary())
            .child(text.chars().take(2).collect::<String>().to_uppercase())
            .into_any_element(),
    }
}

/// The same leading visual as a menu row, for the control that opens it.
///
/// A cell and its menu showing the same glyph is what makes the assignment
/// legible without opening anything.
pub fn cell_glyph(glyph: &MenuGlyph, tint: gpui::Rgba) -> gpui::AnyElement {
    render_glyph(glyph, tint)
}

pub fn combo_box_string_menu(
    id: impl Into<gpui::ElementId>,
    position: crate::overlay::OverlayPosition,
    selected: &str,
    options: &[String],
    on_select: Arc<dyn Fn(String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let items: Vec<MenuItem> = dedupe_preserve_order(options)
        .into_iter()
        .map(MenuItem::plain)
        .collect();
    combo_box_icon_menu(id, position, selected, &items, on_select)
}

/// The same menu, with a leading glyph per row.
///
/// Rows are deduplicated by **label**, exactly as the string form is: the label
/// is what the caller's lookup is keyed by, so two rows that read the same are
/// one row however differently they are illustrated.
pub fn combo_box_icon_menu(
    id: impl Into<gpui::ElementId>,
    position: crate::overlay::OverlayPosition,
    selected: &str,
    items: &[MenuItem],
    on_select: Arc<dyn Fn(String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let options: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
    let deduped: Vec<MenuItem> = {
        let mut seen = HashSet::new();
        items
            .iter()
            .filter(|item| seen.insert(item.label.clone()))
            .cloned()
            .collect()
    };
    combobox_debug(&format!(
        "open options={} unique={} selected={selected}",
        options.len(),
        deduped.len()
    ));
    let left: f32 = position.x.into();
    let top: f32 = position.y.into();
    let width: f32 = position.width.map(|w| w.into()).unwrap_or(120.0);
    let max_h: f32 = position.max_height.map(|h| h.into()).unwrap_or(148.0);
    #[cfg(target_os = "windows")]
    let platform = "windows";
    #[cfg(not(target_os = "windows"))]
    let platform = "other";
    combobox_debug(&format!(
        "menu_bounds platform={platform} x={left:.0} y={top:.0} w={width:.0} max_h={max_h:.0}"
    ));
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .max_h(px(max_h))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_card())
        .shadow(vec![gpui::BoxShadow {
            color: Colors::surface_overlay().into(),
            offset: gpui::point(px(0.0), px(6.0)),
            blur_radius: px(18.0),
            spread_radius: px(0.0),
            inset: false,
        }])
        .p(px(4.0))
        .id(id)
        .overflow_y_scroll()
        .occlude()
        .children(deduped.iter().enumerate().map(|(index, item)| {
            let option = &item.label;
            let active = option == selected;
            let value = option.clone();
            let on_select = on_select.clone();
            let tint = if active {
                Colors::accent_primary()
            } else {
                Colors::text_muted()
            };
            div()
                .id(("combo-box-string-option", index))
                .min_h(px(25.0))
                .w_full()
                .rounded(px(crate::theme::radius::CONTROL))
                .px(px(8.0))
                .py(px(4.0))
                .flex()
                .items_center()
                .gap(px(7.0))
                .bg(if active {
                    Colors::accent_muted()
                } else {
                    gpui::transparent_black().into()
                })
                .text_size(px(10.5))
                .font_weight(if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if active {
                    Colors::text_primary()
                } else {
                    Colors::text_secondary()
                })
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(|s| s.bg(Colors::surface_control_hover()))
                .on_click(move |_, window, cx| on_select(value.clone(), window, cx))
                .child(render_glyph(&item.glyph, tint))
                .child(
                    div()
                        .min_w(px(0.0))
                        .flex_1()
                        .truncate()
                        .child(option.clone()),
                )
                .children(active.then(|| {
                    svg()
                        .path(assets::ICON_CHECK_PATH)
                        .w(px(11.0))
                        .h(px(11.0))
                        .flex_shrink_0()
                        .text_color(Colors::accent_primary())
                }))
        }))
}

/// One node of a [`combo_box_tree_menu`].
#[derive(Clone)]
pub struct TreeMenuNode {
    pub label: String,
    pub glyph: MenuGlyph,
    pub kind: TreeMenuNodeKind,
}

#[derive(Clone)]
pub enum TreeMenuNodeKind {
    /// A choice. Choosing it hands `value` to the caller; it is also what the
    /// selected row is matched by, so two rows may read the same.
    Leaf { value: String },
    /// A heading over its children. Clicking it folds or unfolds them.
    /// `empty` is the line shown in place of children when there are none.
    Branch {
        children: Vec<TreeMenuNode>,
        empty: String,
    },
}

impl TreeMenuNode {
    pub fn leaf(label: impl Into<String>, value: impl Into<String>, glyph: MenuGlyph) -> Self {
        Self {
            label: label.into(),
            glyph,
            kind: TreeMenuNodeKind::Leaf {
                value: value.into(),
            },
        }
    }

    pub fn branch(
        label: impl Into<String>,
        glyph: MenuGlyph,
        children: Vec<TreeMenuNode>,
        empty: impl Into<String>,
    ) -> Self {
        Self {
            label: label.into(),
            glyph,
            kind: TreeMenuNodeKind::Branch {
                children,
                empty: empty.into(),
            },
        }
    }
}

/// Which branches of one tree menu the user folded. Element state, so a menu
/// remembers it while it stays open and nothing outside it has to.
#[derive(Default)]
struct TreeMenuState {
    folded: HashSet<String>,
}

/// Indent per tree level, in logical pixels: a row's chevron and its gap, so a
/// child's glyph lines up under its parent's.
const TREE_INDENT: f32 = 14.0;

/// A menu whose choices are grouped under headings that fold, for a choice
/// that comes from more than one kind of source.
pub fn combo_box_tree_menu(
    id: impl Into<gpui::ElementId>,
    position: crate::overlay::OverlayPosition,
    selected: &str,
    nodes: Vec<TreeMenuNode>,
    on_select: Arc<dyn Fn(String, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    TreeMenu {
        id: id.into(),
        position,
        selected: selected.to_string(),
        nodes,
        on_select,
    }
}

#[derive(gpui::IntoElement)]
struct TreeMenu {
    id: gpui::ElementId,
    position: crate::overlay::OverlayPosition,
    selected: String,
    nodes: Vec<TreeMenuNode>,
    on_select: Arc<dyn Fn(String, &mut Window, &mut App) + 'static>,
}

impl gpui::RenderOnce for TreeMenu {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| TreeMenuState::default());
        let folded = state.read(cx).folded.clone();
        let mut rows = Vec::new();
        let walk = TreeWalk {
            selected: &self.selected,
            folded: &folded,
            state: &state,
            on_select: &self.on_select,
        };
        walk.rows(&self.nodes, 0, "", &mut rows);
        let left: f32 = self.position.x.into();
        let top: f32 = self.position.y.into();
        let width: f32 = self.position.width.map(|w| w.into()).unwrap_or(120.0);
        let max_h: f32 = self.position.max_height.map(|h| h.into()).unwrap_or(148.0);
        div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(width))
            .max_h(px(max_h))
            .rounded(px(crate::theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_card())
            .shadow(vec![gpui::BoxShadow {
                color: Colors::surface_overlay().into(),
                offset: gpui::point(px(0.0), px(6.0)),
                blur_radius: px(18.0),
                spread_radius: px(0.0),
                inset: false,
            }])
            .p(px(4.0))
            .id(self.id)
            .overflow_y_scroll()
            .occlude()
            .children(rows)
    }
}

/// What every row of one tree menu shares while it is flattened.
struct TreeWalk<'a> {
    selected: &'a str,
    folded: &'a HashSet<String>,
    state: &'a gpui::Entity<TreeMenuState>,
    on_select: &'a Arc<dyn Fn(String, &mut Window, &mut App) + 'static>,
}

impl TreeWalk<'_> {
    /// Flatten `nodes` into menu rows, depth first, skipping folded branches.
    /// `path` names the branch being walked so two headings that read the
    /// same fold independently.
    fn rows(
        &self,
        nodes: &[TreeMenuNode],
        depth: usize,
        path: &str,
        rows: &mut Vec<gpui::AnyElement>,
    ) {
        let indent = px(8.0 + TREE_INDENT * depth as f32);
        for node in nodes {
            let index = rows.len();
            match &node.kind {
                TreeMenuNodeKind::Leaf { value } => {
                    rows.push(self.leaf(node, value, index, indent));
                }
                TreeMenuNodeKind::Branch { children, empty } => {
                    let key = format!("{path}/{}", node.label);
                    let open = !self.folded.contains(&key);
                    rows.push(self.heading(node, &key, open, children.len(), index, indent));
                    if !open {
                        continue;
                    }
                    if children.is_empty() {
                        rows.push(
                            div()
                                .min_h(px(22.0))
                                .w_full()
                                .pl(px(8.0 + TREE_INDENT * (depth + 1) as f32))
                                .pr(px(8.0))
                                .flex()
                                .items_center()
                                .text_size(px(10.0))
                                .text_color(Colors::text_faint())
                                .child(empty.clone())
                                .into_any_element(),
                        );
                    } else {
                        self.rows(children, depth + 1, &key, rows);
                    }
                }
            }
        }
    }

    fn leaf(
        &self,
        node: &TreeMenuNode,
        value: &str,
        index: usize,
        indent: gpui::Pixels,
    ) -> gpui::AnyElement {
        let active = value == self.selected;
        let tint = if active {
            Colors::accent_primary()
        } else {
            Colors::text_muted()
        };
        let value = value.to_string();
        let on_select = self.on_select.clone();
        div()
            .id(("combo-box-tree-row", index))
            .min_h(px(25.0))
            .w_full()
            .rounded(px(crate::theme::radius::CONTROL))
            .pl(indent)
            .pr(px(8.0))
            .py(px(4.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(if active {
                Colors::accent_muted()
            } else {
                gpui::transparent_black().into()
            })
            .text_size(px(10.5))
            .font_weight(if active {
                gpui::FontWeight::SEMIBOLD
            } else {
                gpui::FontWeight::NORMAL
            })
            .text_color(if active {
                Colors::text_primary()
            } else {
                Colors::text_secondary()
            })
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| s.bg(Colors::surface_control_hover()))
            .on_click(move |_, window, cx| on_select(value.clone(), window, cx))
            .child(render_glyph(&node.glyph, tint))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .child(node.label.clone()),
            )
            .children(active.then(|| {
                svg()
                    .path(assets::ICON_CHECK_PATH)
                    .w(px(11.0))
                    .h(px(11.0))
                    .flex_shrink_0()
                    .text_color(Colors::accent_primary())
            }))
            .into_any_element()
    }

    fn heading(
        &self,
        node: &TreeMenuNode,
        key: &str,
        open: bool,
        count: usize,
        index: usize,
        indent: gpui::Pixels,
    ) -> gpui::AnyElement {
        let state = self.state.clone();
        let key = key.to_string();
        div()
            .id(("combo-box-tree-row", index))
            .h(px(24.0))
            .w_full()
            .rounded(px(crate::theme::radius::CONTROL))
            .pl(indent)
            .pr(px(8.0))
            .flex()
            .items_center()
            .gap(px(5.0))
            .text_size(px(10.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_muted())
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| s.bg(Colors::surface_control_hover()))
            .on_click(move |_, _, cx| {
                state.update(cx, |s, cx| {
                    if !s.folded.remove(&key) {
                        s.folded.insert(key.clone());
                    }
                    cx.notify();
                });
            })
            .child(
                svg()
                    .path(if open {
                        assets::ICON_CHEVRON_DOWN_PATH
                    } else {
                        assets::ICON_CHEVRON_RIGHT_PATH
                    })
                    .w(px(9.0))
                    .h(px(9.0))
                    .flex_shrink_0()
                    .text_color(Colors::text_faint()),
            )
            .child(render_glyph(&node.glyph, Colors::text_muted()))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .child(node.label.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(gpui::FontWeight::NORMAL)
                    .text_color(Colors::text_faint())
                    .child(count.to_string()),
            )
            .into_any_element()
    }
}
