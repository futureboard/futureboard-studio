//! The arrangement's track header.
//!
//! ```txt
//! ┌──┬──────────────────────────────────────────────┬──┐
//! │ 1│ [M|S|●|◐]  Name ··········· [♪] [A|❄]         │▮▮│
//! │ ⌁│ ━━━━━━━━━━━━●──────────  -3.0 dB   (◜)  L20    │▮▮│
//! └──┴──────────────────────────────────────────────┴──┘
//!  tab  the toggles the hand plays first, then the name;   meter
//!       lane modes at the end; level and the pan knob below
//! ```
//!
//! The tab carries the track's number and colour and the glyph of its type,
//! and fills with the colour while the track is selected. Below the default
//! row height the header keeps only its first line.

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, svg, AppContext, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::components::controls::fb_tooltip;
use crate::components::fader::slim_horizontal_fader_with_drag_callbacks;
use crate::components::knob::{format_pan_label, knob_bipolar_with_gesture, KnobGesture};
use crate::components::text_input::{
    text_field_with_callbacks_and_ime, TextInputCallbacks, TextInputState,
};
use crate::components::timeline::arrangement_overlay::{
    track_meter_slot, SharedArrangementOverlay,
};
use crate::components::timeline::timeline_state::{
    volume, GroupTree, TimelineState, TrackDragItem, TrackLaneMode, TrackState, TrackType,
    HEADER_WIDTH, TRACK_HEADER_CONTROLS_MIN_HEIGHT,
};
use crate::components::timeline::vu_meter::HEADER_METER_COLUMN_W;
use crate::theme::{radius, size, space, typography, Colors};

type TrackCallback = std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>;
type TrackSelectCallback =
    std::sync::Arc<dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static>;
type VolumeCallback =
    std::sync::Arc<dyn Fn(&(String, f32), &mut gpui::Window, &mut gpui::App) + 'static>;
type VolumeCommitCallback =
    std::sync::Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>;
type TrackContextCallback =
    std::sync::Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>;
type TrackGroupCallback =
    std::sync::Arc<dyn Fn(&(String, String), &mut gpui::Window, &mut gpui::App) + 'static>;
/// `(track_id, take_id)`.
type TrackTakeCallback =
    std::sync::Arc<dyn Fn(&(String, String), &mut gpui::Window, &mut gpui::App) + 'static>;

/// Bundle of callbacks the TrackHeader can fire. Keeping them in one struct
/// keeps the function signature manageable and lets new actions land without
/// re-threading every call site.
#[derive(Clone)]
pub struct TrackHeaderCallbacks {
    pub on_select_track: TrackSelectCallback,
    pub on_toggle_mute: TrackCallback,
    pub on_toggle_solo: TrackCallback,
    pub on_toggle_arm: TrackCallback,
    pub on_toggle_input: TrackCallback,
    /// Toggle the track between Clip and Automation edit mode.
    pub on_toggle_automation: TrackCallback,
    pub on_volume_change: VolumeCallback,
    pub on_volume_drag_start: VolumeCallback,
    pub on_volume_drag_preview: VolumeCallback,
    pub on_volume_drag_commit: VolumeCommitCallback,
    /// Discrete pan changes such as double-click reset.
    pub on_pan_change: VolumeCallback,
    pub on_pan_drag_start: TrackCallback,
    pub on_pan_drag_preview: VolumeCallback,
    pub on_pan_drag_commit: VolumeCommitCallback,
    pub on_assign_to_group: TrackGroupCallback,
    pub on_toggle_group_collapsed: TrackCallback,
    pub on_context_menu: Option<TrackContextCallback>,
    /// Open/close the track's take sub-lane.
    pub on_toggle_takes: TrackCallback,
    /// Make a take the one that is heard: `(track_id, take_id)`.
    pub on_select_take: TrackTakeCallback,
    /// Delete a take and the clip it holds: `(track_id, take_id)`.
    pub on_delete_take: TrackTakeCallback,
    /// Open the track's instrument plugin editor, or the instrument picker when
    /// the slot is empty. `None` hides the header button.
    pub on_open_instrument: Option<TrackCallback>,
    /// Freeze the track, or unfreeze a frozen one. `None` hides the toggle.
    pub on_toggle_freeze: Option<TrackCallback>,
    /// Open the inline name editor on a plain double-click of the name; see
    /// [`opens_track_rename`].
    pub on_begin_rename: TrackCallback,
    /// The rename in progress, drawn in place of its track's name. `None` when
    /// no header is being renamed, so the other rows capture nothing of it.
    pub rename: Option<std::rc::Rc<TrackHeaderRename>>,
}

/// Height of the inline name field: between the 20 px affordances beside the
/// name and the 24 px band of the M/S/R/I/A strip, so opening the editor never
/// changes the height of the name line, on a compact row or a full one.
pub const TRACK_NAME_FIELD_HEIGHT: f32 = 22.0;

/// Text inset of the inline name field.
pub const TRACK_NAME_FIELD_PAD_X: f32 = 4.0;

/// How far the inline name field reaches left of the name: its border and
/// inset, so its text starts exactly where the name's did.
const TRACK_NAME_FIELD_INSET: f32 = 1.0 + TRACK_NAME_FIELD_PAD_X;

/// What the header needs to draw the name editor the arrangement owns.
pub struct TrackHeaderRename {
    pub track_id: String,
    pub input: TextInputState,
    pub focused: bool,
    pub callbacks: TextInputCallbacks,
    pub ime_target: gpui::Entity<crate::components::timeline::Timeline>,
}

/// Whether a click on a track name opens the inline rename.
///
/// Only a plain, stationary double-click does. The name sits inside the drag
/// zone, and GPUI still delivers a child's click after the parent dragged, so
/// the click count alone would open the editor after two quick reorder drags;
/// [`is_reset_double_click`](crate::components::fader::is_reset_double_click)
/// requires the pointer to have stayed put. A modified double-click is the
/// header's Cmd/Shift selection gesture pressed twice, not a request to rename.
pub fn opens_track_rename(event: &gpui::ClickEvent, drag_active: bool) -> bool {
    let modified = match event {
        gpui::ClickEvent::Mouse(click) => {
            click.down.modifiers.modified() || click.up.modifiers.modified()
        }
        gpui::ClickEvent::Keyboard(_) => false,
    };
    !drag_active && !modified && crate::components::fader::is_reset_double_click(event)
}

pub struct TrackDragPreview {
    pub name: String,
    pub color: gpui::Rgba,
}

impl Render for TrackDragPreview {
    fn render(&mut self, _w: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(space::SNUG))
            .h(px(size::COMFORTABLE))
            .min_w(px(150.0))
            .px(px(space::BASE))
            .rounded(px(radius::CONTROL))
            .border(px(1.0))
            .border_color({
                let mut c = self.color;
                c.a = 0.72;
                c
            })
            .bg(Colors::surface_raised())
            .shadow_lg()
            .child(
                div()
                    .w(px(3.0))
                    .h(px(size::MICRO))
                    .rounded(px(radius::PILL))
                    .bg(self.color),
            )
            .child(
                div()
                    .max_w(px(220.0))
                    .overflow_hidden()
                    .truncate()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Colors::text_primary())
                    .child(self.name.clone()),
            )
    }
}

/// Glyph sizes inside the header's controls. Two only: one for a 16 px pill and
/// one for the 20-22 px affordances beside the name. A third size here is what
/// makes a compact row look assembled from spare parts.
const GLYPH_SM: f32 = 10.0;
const GLYPH_MD: f32 = 11.0;

/// How far each folder level indents a row, and where the outermost folder's
/// rail sits: just past the row's number tab. A row inside
/// folders draws one rail per level, in that folder's colour, so the tree reads
/// down the column the way the rows stack, at any depth, with no frame to
/// break where a member's own folder starts or ends.
const FOLDER_INDENT: f32 = space::LOOSE;
const FOLDER_RAIL_X: f32 = TRACK_TAB_W + space::TIGHT;
const FOLDER_RAIL_W: f32 = 2.0;
/// The deepest level that still indents. Deeper rows line up with it, so a
/// deep tree cannot push the name and controls out of the fixed-width column;
/// their rails still show every level.
const FOLDER_MAX_INDENT_LEVELS: usize = 6;

/// Where a row sits among folders. Read once per frame from the
/// [`GroupTree`], rather than by each header scanning the track list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FolderPlacement {
    /// The colours of the folders enclosing the row, outermost first.
    pub rails: Vec<gpui::Rgba>,
    /// For a folder: how many tracks sit directly inside it.
    pub members: usize,
}

impl FolderPlacement {
    pub fn of(state: &TimelineState, tree: &GroupTree, index: usize) -> Self {
        let members = if state
            .tracks
            .get(index)
            .is_some_and(|track| track.track_type == TrackType::Group)
        {
            tree.member_count(index)
        } else {
            0
        };
        Self::with_members(state, tree, index, members)
    }

    pub(crate) fn with_members(
        state: &TimelineState,
        tree: &GroupTree,
        index: usize,
        members: usize,
    ) -> Self {
        let rails = tree
            .ancestors(index)
            .into_iter()
            .rev()
            .map(|folder| state.tracks[folder].color)
            .collect();
        let is_folder = state
            .tracks
            .get(index)
            .is_some_and(|track| track.track_type == TrackType::Group);
        Self {
            rails,
            members: if is_folder { members } else { 0 },
        }
    }

    fn indent(&self) -> f32 {
        self.rails.len().min(FOLDER_MAX_INDENT_LEVELS) as f32 * FOLDER_INDENT
    }
}

/// A collapsed folder's member count, beside its name: what the folded rows
/// hold, without opening it.
fn folder_member_badge(members: usize) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(space::TIGHT))
        .h(px(size::MICRO))
        .flex()
        .items_center()
        .rounded(px(radius::CONTROL_SM))
        .bg(Colors::surface_badge())
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_muted())
        .child(if members == 1 {
            "1 track".to_string()
        } else {
            format!("{members} tracks")
        })
}

/// The folder header's menu button: the track context menu, where the folder
/// commands live, one click from the row that owns them.
fn folder_menu_button(
    id_num: usize,
    track_id: String,
    on_open: TrackContextCallback,
) -> impl IntoElement {
    let rest = Colors::button_bg();
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        // Same hit band as the latch strip beside it.
        .py(px(size::hit_target(size::DENSE)))
        .flex_none()
        .child(
            div()
                .id(("folder-menu", id_num))
                .flex()
                .items_center()
                .justify_center()
                .w(px(size::DENSE))
                .h(px(size::DENSE))
                .rounded(px(radius::CONTROL_SM))
                .bg(rest)
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .cursor(gpui::CursorStyle::PointingHand)
                .hover(move |style| style.bg(hover))
                .active(|style| style.bg(Colors::composite(rest, Colors::state_recessed())))
                .tooltip(fb_tooltip("Folder options"))
                .occlude()
                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                    cx.stop_propagation();
                    let x: f32 = event.position.x.into();
                    let y: f32 = event.position.y.into();
                    on_open(&(track_id.clone(), x, y), window, cx);
                })
                .child(
                    svg()
                        .path(assets::ICON_ELLIPSIS_PATH)
                        .w(px(GLYPH_MD))
                        .h(px(GLYPH_MD))
                        .text_color(Colors::text_muted()),
                ),
        )
}

/// What a toggle shows: a letter, an icon, or the record dot.
#[derive(Clone, Copy)]
enum SegmentGlyph {
    Letter(&'static str),
    Icon(&'static str),
    Dot,
}

/// Diameter of the record dot.
const RECORD_DOT: f32 = 7.0;

/// One latching toggle inside a touching group.
///
/// The toggles of a group share one hairline instead of each carrying its own
/// border, and only the group's outer corners round: the group clips them, so
/// a shared edge never leaves a notch of background between two arcs.
///
/// Latched state is carried on fill *and* glyph — `Colors::latched`'s wash
/// under the semantic hue at full strength. The third channel, the latched
/// border, moves to the dividers: the hairline next to a latched toggle takes
/// that hue (see [`latch_divider`]). `divider` is `None` for the first
/// toggle. One that is not `enabled` keeps its place, dimmed, takes no press,
/// and its tooltip says why.
#[allow(clippy::too_many_arguments)]
fn latch_segment(
    id: gpui::ElementId,
    glyph: SegmentGlyph,
    tooltip: String,
    active: bool,
    enabled: bool,
    semantic: gpui::Rgba,
    divider: Option<gpui::Rgba>,
    on_click: LatchPressHandler,
) -> impl IntoElement {
    let base = Colors::surface_panel_alt();
    let rest = if active {
        Colors::latched(base, semantic).0
    } else {
        Colors::button_bg()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let fg = if !enabled {
        Colors::text_disabled()
    } else if active {
        semantic
    } else {
        Colors::text_muted()
    };

    let seg = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .w(px(size::DENSE))
        .h(px(size::DENSE))
        .bg(rest)
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::BOLD)
        .text_color(fg)
        .tooltip(fb_tooltip(tooltip))
        .when(enabled, |seg| {
            seg.cursor(gpui::CursorStyle::PointingHand)
                .hover(move |s| s.bg(hover))
                .on_mouse_down(gpui::MouseButton::Left, on_click)
        })
        .when(!enabled, |seg| {
            seg.on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        })
        .when_some(divider, |seg, color| {
            seg.border_l(px(1.0)).border_color(color)
        });

    match glyph {
        SegmentGlyph::Letter(label) => seg.child(label),
        SegmentGlyph::Icon(path) => seg.child(
            svg()
                .path(path)
                .w(px(GLYPH_SM))
                .h(px(GLYPH_SM))
                .text_color(fg),
        ),
        SegmentGlyph::Dot => seg.child(div().size(px(RECORD_DOT)).rounded(px(radius::PILL)).bg(fg)),
    }
}

/// Colour of the hairline between two toggles.
///
/// A divider that touches a latched toggle takes that toggle's latched border
/// hue, so the group still encodes state on a border channel even though no
/// toggle owns one.
fn latch_divider(left: Option<gpui::Rgba>, right: Option<gpui::Rgba>) -> gpui::Rgba {
    let base = Colors::surface_panel_alt();
    match (left, right) {
        (Some(hue), _) | (None, Some(hue)) => Colors::latched(base, hue).1,
        (None, None) => Colors::border_subtle(),
    }
}

type LatchPressHandler =
    Box<dyn Fn(&gpui::MouseDownEvent, &mut gpui::Window, &mut gpui::App) + 'static>;

/// The frame of a touching group. It owns the border, the corner radius and
/// the hit area: `size::hit_target` grows a control with transparent padding,
/// and padding on a toggle would push it off its neighbour, so the toggles
/// are `size::DENSE` (20 px) squares and the group carries the 2 px above and
/// below that lifts the row to the 24 px comfortable target.
fn touching_group(inner: gpui::Div) -> impl IntoElement {
    div()
        .py(px(size::hit_target(size::DENSE)))
        .flex_none()
        .child(
            inner
                .flex()
                .flex_row()
                .items_center()
                .flex_none()
                .rounded(px(radius::CONTROL_SM))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .overflow_hidden(),
        )
}

/// The press handlers of the state toggles.
struct StateStripHandlers {
    on_mute: LatchPressHandler,
    on_solo: LatchPressHandler,
    on_arm: LatchPressHandler,
    on_input: LatchPressHandler,
}

/// Mute, Solo, Record and Monitor: the toggles a player hits by muscle
/// memory, first in the row so they sit in the same place on every track
/// whatever its name. A folder has no input, so it keeps Mute and Solo.
fn state_strip(
    track: &TrackState,
    id_num: usize,
    handlers: StateStripHandlers,
) -> impl IntoElement {
    let is_folder = track.track_type == TrackType::Group;
    let frozen = track.is_frozen();
    let monitoring = track.input_monitor.is_active(track.armed);
    let lit = [
        track.muted.then(Colors::state_mute),
        track.solo.then(Colors::state_solo),
        track.armed.then(Colors::state_arm),
        monitoring.then(Colors::state_monitor),
    ];
    // A frozen track plays its bounce: there is nothing to record into or
    // monitor until it is unfrozen.
    let unless_frozen = |label: &str| {
        if frozen {
            format!("{label} (unfreeze the track first)")
        } else {
            label.to_string()
        }
    };
    let StateStripHandlers {
        on_mute,
        on_solo,
        on_arm,
        on_input,
    } = handlers;

    touching_group(
        div()
            .child(latch_segment(
                ("mute-btn", id_num).into(),
                SegmentGlyph::Letter("M"),
                "Mute".to_string(),
                track.muted,
                true,
                Colors::state_mute(),
                None,
                on_mute,
            ))
            .child(latch_segment(
                ("solo-btn", id_num).into(),
                SegmentGlyph::Letter("S"),
                "Solo".to_string(),
                track.solo,
                true,
                Colors::state_solo(),
                Some(latch_divider(lit[0], lit[1])),
                on_solo,
            ))
            .when(!is_folder, |strip| {
                strip
                    .child(latch_segment(
                        ("arm-btn", id_num).into(),
                        SegmentGlyph::Dot,
                        unless_frozen("Record Arm"),
                        track.armed,
                        !frozen,
                        Colors::state_arm(),
                        Some(latch_divider(lit[1], lit[2])),
                        on_arm,
                    ))
                    .child(latch_segment(
                        ("input-btn", id_num).into(),
                        SegmentGlyph::Icon(assets::ICON_VOLUME_2_PATH),
                        unless_frozen("Input Monitoring"),
                        monitoring,
                        !frozen,
                        Colors::state_monitor(),
                        Some(latch_divider(lit[2], lit[3])),
                        on_input,
                    ))
            }),
    )
}

/// Automation and Freeze: modes of the lane rather than of the signal, at
/// the row's end, apart from the toggles played during a take.
fn mode_strip(
    track: &TrackState,
    is_automation: bool,
    id_num: usize,
    on_automation: LatchPressHandler,
    on_freeze: Option<LatchPressHandler>,
) -> impl IntoElement {
    let frozen = track.is_frozen();
    touching_group(
        div()
            .child(latch_segment(
                ("auto-btn", id_num).into(),
                SegmentGlyph::Icon(assets::ICON_AUTOMATION_PATH),
                if is_automation {
                    "Automation: showing the lane's automation".to_string()
                } else {
                    "Automation".to_string()
                },
                is_automation,
                true,
                Colors::state_automation(),
                None,
                on_automation,
            ))
            .when_some(on_freeze, |strip, on_freeze| {
                strip.child(latch_segment(
                    ("freeze-btn", id_num).into(),
                    SegmentGlyph::Icon(assets::ICON_SNOWFLAKE_PATH),
                    if frozen {
                        "Unfreeze Track".to_string()
                    } else {
                        "Freeze Track: render it and turn its plug-ins off".to_string()
                    },
                    frozen,
                    true,
                    Colors::state_freeze(),
                    Some(latch_divider(
                        is_automation.then(Colors::state_automation),
                        frozen.then(Colors::state_freeze),
                    )),
                    on_freeze,
                ))
            }),
    )
}

/// The glyph for a track's type, shown in its tab.
fn track_type_icon(track_type: TrackType) -> &'static str {
    match track_type {
        TrackType::Audio => assets::ICON_AUDIO_LINES_PATH,
        TrackType::Midi => assets::ICON_MUSIC_PATH,
        TrackType::Instrument => assets::ICON_KEYBOARD_PATH,
        TrackType::Bus => assets::ICON_GIT_MERGE_PATH,
        TrackType::Return => assets::ICON_ROUTE_PATH,
        TrackType::Group => assets::ICON_FOLDER_PATH,
        TrackType::Master => assets::ICON_SLIDERS_HORIZONTAL_PATH,
        TrackType::Video => assets::ICON_FILM_PATH,
    }
}

fn track_type_name(track_type: TrackType) -> &'static str {
    match track_type {
        TrackType::Audio => "Audio track",
        TrackType::Midi => "MIDI track",
        TrackType::Instrument => "Instrument track",
        TrackType::Bus => "Bus",
        TrackType::Return => "FX return",
        TrackType::Group => "Folder",
        TrackType::Master => "Master",
        TrackType::Video => "Video track",
    }
}

/// Width of the number tab at the header's left edge.
pub const TRACK_TAB_W: f32 = 24.0;
/// The solid edge of the tab, in line with the lane's own colour stripe.
const TAB_EDGE_W: f32 = 3.0;
/// Below this row height the tab shows the number alone.
const TAB_GLYPH_MIN_ROW: f32 = 44.0;

/// The number tab: the track's position, its colour, and the glyph of its
/// type. Selected, it fills with the colour, the strongest cue in the column
/// for which rows the next command acts on. It is a drag handle too.
fn number_tab(
    track: &TrackState,
    number: usize,
    selected: bool,
    row_height: f32,
    id_num: usize,
    drag: TrackDragItem,
) -> impl IntoElement {
    let fill = if selected {
        track.color
    } else {
        Colors::with_alpha(track.color, 0.16)
    };
    let ink = if selected {
        Colors::text_inverse()
    } else {
        Colors::text_secondary()
    };
    let glyph = if selected {
        Colors::text_inverse()
    } else {
        track.color
    };
    div()
        .id(("track-tab", id_num))
        .relative()
        .flex_none()
        .w(px(TRACK_TAB_W))
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(space::TIGHT))
        // The number sits on the first line's centre: the row's padding plus
        // half of the 24 px toggle band, less half the number's own height.
        .pt(px(space::SNUG + size::DEFAULT / 2.0 - 7.0))
        .pl(px(TAB_EDGE_W))
        .bg(fill)
        .border_r(px(1.0))
        .border_color(Colors::border_subtle())
        .cursor(gpui::CursorStyle::OpenHand)
        .tooltip(fb_tooltip(format!(
            "{} {number}: drag to reorder",
            track_type_name(track.track_type)
        )))
        .on_drag(drag, |drag, _offset, _window, cx| {
            cx.new(|_| TrackDragPreview {
                name: drag.name.clone(),
                color: drag.color,
            })
        })
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(TAB_EDGE_W))
                .bg(track.color),
        )
        .child(
            div()
                .h(px(14.0))
                .flex()
                .items_center()
                .text_size(px(typography::DENSE_LABEL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ink)
                .child(number.to_string()),
        )
        .when(row_height >= TAB_GLYPH_MIN_ROW, |tab| {
            tab.child(
                svg()
                    .path(track_type_icon(track.track_type))
                    .w(px(GLYPH_MD))
                    .h(px(GLYPH_MD))
                    .text_color(glyph),
            )
        })
}

/// Width of the dB value beside the level bar.
const DB_READOUT_W: f32 = 48.0;
/// Width of the pan value beside the knob.
const PAN_READOUT_W: f32 = 28.0;
/// The pan knob: the mixer's, at the size of the toggles above it.
const PAN_KNOB: f32 = 22.0;
/// Height of the level line: the knob's.
const LEVEL_LINE_H: f32 = PAN_KNOB;

/// A value beside its control: the number, and a quieter unit.
fn readout(width: f32, value: String, unit: Option<&'static str>) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(width))
        .flex()
        .flex_row()
        .items_baseline()
        .justify_end()
        .gap(px(2.0))
        .child(
            div()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(value),
        )
        .children(unit.map(|unit| {
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(unit)
        }))
}

/// Pan as a knob: a vertical drag turns it, Shift turns it finely, and a
/// double-click or Alt-click centres it. One drag is one undo step.
fn pan_knob(
    track: &TrackState,
    on_start: TrackCallback,
    on_preview: VolumeCallback,
    on_commit: VolumeCommitCallback,
    on_change: VolumeCallback,
) -> impl IntoElement {
    let start_id = track.id.clone();
    let end_id = track.id.clone();
    let preview_id = track.id.clone();
    let reset_id = track.id.clone();
    knob_bipolar_with_gesture(
        format!("track-pan-{}", track.id),
        track.pan,
        -1.0,
        1.0,
        PAN_KNOB,
        // The track's colour, as on its mixer strip.
        track.color,
        0.0,
        KnobGesture {
            on_start: Some(std::rc::Rc::new(move |window, cx| {
                on_start(&start_id, window, cx)
            })),
            on_end: Some(std::rc::Rc::new(move |window, cx| {
                on_commit(&end_id, window, cx)
            })),
        },
        move |pan: &f32, window: &mut gpui::Window, cx: &mut gpui::App| {
            on_preview(&(preview_id.clone(), *pan), window, cx)
        },
        move |pan: &f32, window: &mut gpui::Window, cx: &mut gpui::App| {
            on_change(&(reset_id.clone(), *pan), window, cx)
        },
    )
}

/// In place of the pan knob on a channel in Dual pan: a balance value would
/// be wrong, so the header says which mode the channel is in and leaves the
/// two positions to the mixer strip (whose pan right-click switches modes).
/// Spans the knob and its readout, so the row keeps its width.
fn dual_pan_badge(track: &TrackState) -> impl IntoElement {
    let dual = track.dual_pan;
    div()
        .id(gpui::SharedString::from(format!(
            "track-dual-pan-{}",
            track.id
        )))
        .flex_none()
        .w(px(PAN_KNOB + PAN_READOUT_W))
        .h(px(PAN_KNOB))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(radius::CONTROL_SM))
        .bg(Colors::surface_badge())
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(Colors::text_secondary())
        .tooltip(fb_tooltip(format!(
            "Dual pan: L input {} / R input {}. Right-click the mixer pan to switch to Balance.",
            format_pan_label(dual.left),
            format_pan_label(dual.right)
        )))
        .child("Dual")
}

/// The takes chip beside the name: how many takes the track holds, and the
/// switch that opens its take lanes below it (see `take_lane`), where a take
/// is chosen.
fn takes_chip(track: &TrackState, id_num: usize, on_toggle: TrackCallback) -> impl IntoElement {
    let track_id = track.id.clone();
    let open = track.takes_expanded;
    let rest = if open {
        Colors::surface_selected_soft()
    } else {
        Colors::surface_badge()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    div()
        .id(("track-takes-toggle", id_num))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.0))
        .px(px(space::TIGHT))
        .h(px(size::MICRO))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .hover(move |style| style.bg(hover))
        .cursor(gpui::CursorStyle::PointingHand)
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(Colors::text_muted())
        .tooltip(fb_tooltip(if open { "Hide takes" } else { "Show takes" }))
        .child(
            svg()
                .path(if open {
                    assets::ICON_CHEVRON_DOWN_PATH
                } else {
                    assets::ICON_CHEVRON_RIGHT_PATH
                })
                .w(px(GLYPH_SM))
                .h(px(GLYPH_SM))
                .text_color(Colors::text_muted()),
        )
        .child(if track.takes.len() == 1 {
            "1 take".to_string()
        } else {
            format!("{} takes", track.takes.len())
        })
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, move |_event, window, cx| {
            cx.stop_propagation();
            on_toggle(&track_id, window, cx);
        })
}

/// Compact instrument affordance on an Instrument track header.
///
/// Click opens the loaded plugin editor, Solfege dock, or Soundfont Player —
/// or the instrument picker when the slot is still empty. Icon-only so the
/// name row stays the track name, not a second plugin label.
fn instrument_header_button(
    track: &TrackState,
    id_num: usize,
    on_open: TrackCallback,
) -> impl IntoElement {
    let loaded = track.solfege.is_some()
        || track.builtin_soundfont_player
        || track
            .instrument_insert()
            .is_some_and(|slot| !slot.is_empty());
    let tooltip = if track.solfege.is_some() {
        "Solfege".to_string()
    } else if track.builtin_soundfont_player {
        "Soundfont Player".to_string()
    } else if let Some(slot) = track.instrument_insert().filter(|slot| !slot.is_empty()) {
        slot.display_name.clone()
    } else {
        "Instrument".to_string()
    };
    let rest = if loaded {
        Colors::accent_muted()
    } else {
        Colors::button_bg()
    };
    let hover = Colors::composite(rest, Colors::state_hover());
    let fg = if loaded {
        Colors::accent_primary()
    } else {
        Colors::text_muted()
    };
    let track_id = track.id.clone();

    div()
        .id(("track-instrument", id_num))
        .flex()
        .items_center()
        .justify_center()
        .w(px(size::DENSE))
        .h(px(size::DENSE))
        .rounded(px(radius::CONTROL_SM))
        .bg(rest)
        .border(px(1.0))
        .border_color(if loaded {
            Colors::with_alpha(Colors::accent_primary(), 0.45)
        } else {
            Colors::border_subtle()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |style| style.bg(hover))
        .tooltip(fb_tooltip(tooltip))
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            on_open(&track_id, window, cx);
        })
        .child(
            svg()
                .path(assets::ICON_KEYBOARD_PATH)
                .w(px(GLYPH_MD))
                .h(px(GLYPH_MD))
                .text_color(fg),
        )
}

#[allow(clippy::too_many_arguments)]
pub fn track_header(
    track: &TrackState,
    index: usize,
    state: &TimelineState,
    row_height: f32,
    folder: &FolderPlacement,
    callbacks: TrackHeaderCallbacks,
    meters: &SharedArrangementOverlay,
) -> impl IntoElement {
    let _s = crate::perf::PerfScope::enter("TrackHeader");
    let track_id = track.id.clone();
    // The arrangement shows a marquee's preview while one is in flight.
    let is_selected = state.display_selection().is_track_selected(&track.id);
    let is_automation = track.lane_mode == TrackLaneMode::Automation;
    let is_group = track.track_type == TrackType::Group;
    let folder_menu = callbacks
        .on_context_menu
        .clone()
        .filter(|_| is_group)
        .map(|on_open| (track.id.clone(), on_open));
    // Adaptive header: the volume/pan/meter/dB control row only fits at the
    // default row height or taller. Below that we show the compact single-row
    // header so controls never overlap, clip, or float outside the row.
    let show_controls = row_height >= TRACK_HEADER_CONTROLS_MIN_HEIGHT;
    let is_dragging = state.dragging_track_id.as_deref() == Some(track.id.as_str());
    // Selection, drag, and drop-target are *tints*: translucent washes designed
    // to sit on the header's surface, not to be it. Using one as the whole
    // background left a selected header with no opaque pixel of its own, so
    // whatever the arrangement painted behind the header column showed straight
    // through it. The base stays opaque and the tint goes on top.
    let header_tint = if is_dragging {
        Some(Colors::with_alpha(Colors::text_primary(), 0.07))
    } else if is_automation {
        // Quiet graphite tint so the active automation track reads as active
        // without flooding the header with accent hue.
        Some(Colors::surface_selected_soft())
    } else if is_selected {
        Some(Colors::track_selected_overlay())
    } else if track.is_frozen() {
        // The same frost wash the frozen lane carries, so header and lane read
        // as one locked row.
        Some(Colors::with_alpha(
            Colors::state_freeze(),
            crate::theme::state::ARMED_WASH,
        ))
    } else {
        None
    };
    // A folder's own row sits one plane up from the tracks inside it, so the
    // row that carries the whole folder's fader reads as its head.
    let header_base = if is_group {
        Colors::surface_panel_raised()
    } else {
        Colors::surface_panel()
    };
    let id_num = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        track.id.hash(&mut hasher);
        hasher.finish() as usize
    };

    // Build click handlers up-front so the closure types stay simple.
    let select_id = track_id.clone();
    let on_select_root = {
        let cb = callbacks.on_select_track.clone();
        move |event: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(
                &(
                    select_id.clone(),
                    event.modifiers.control || event.modifiers.platform,
                    event.modifiers.shift,
                ),
                window,
                cx,
            );
        }
    };

    let mute_id = track_id.clone();
    let on_mute = {
        let cb = callbacks.on_toggle_mute.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            // A control press is not a selection press: the row would narrow
            // a multi-selection to this track before the toggle reached them.
            cx.stop_propagation();
            cb(&mute_id, window, cx);
        }
    };

    let solo_id = track_id.clone();
    let on_solo = {
        let cb = callbacks.on_toggle_solo.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            // A control press is not a selection press: the row would narrow
            // a multi-selection to this track before the toggle reached them.
            cx.stop_propagation();
            cb(&solo_id, window, cx);
        }
    };

    let arm_id = track_id.clone();
    let on_arm = {
        let cb = callbacks.on_toggle_arm.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            // A control press is not a selection press: the row would narrow
            // a multi-selection to this track before the toggle reached them.
            cx.stop_propagation();
            cb(&arm_id, window, cx);
        }
    };

    let input_id = track_id.clone();
    let on_input = {
        let cb = callbacks.on_toggle_input.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            // A control press is not a selection press: the row would narrow
            // a multi-selection to this track before the toggle reached them.
            cx.stop_propagation();
            cb(&input_id, window, cx);
        }
    };

    let automation_id = track_id.clone();
    let on_automation = {
        let cb = callbacks.on_toggle_automation.clone();
        move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            // A control press is not a selection press: the row would narrow
            // a multi-selection to this track before the toggle reached them.
            cx.stop_propagation();
            cb(&automation_id, window, cx);
        }
    };

    let on_freeze = callbacks
        .on_toggle_freeze
        .clone()
        .filter(|_| track.track_type.can_freeze())
        .map(|cb| {
            let freeze_id = track_id.clone();
            Box::new(
                move |_: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
                    cx.stop_propagation();
                    cb(&freeze_id, window, cx);
                },
            ) as LatchPressHandler
        });

    let vol_id = track_id.clone();
    let on_volume_drag_start = {
        let cb = callbacks.on_volume_drag_start.clone();
        let vol_id = vol_id.clone();
        move |new_norm: &f32, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&(vol_id.clone(), *new_norm), window, cx);
        }
    };
    let on_volume_drag_preview = {
        let cb = callbacks.on_volume_drag_preview.clone();
        let vol_id = vol_id.clone();
        move |new_norm: &f32, window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&(vol_id.clone(), *new_norm), window, cx);
        }
    };
    let on_volume_drag_commit = {
        let cb = callbacks.on_volume_drag_commit.clone();
        let vol_id = vol_id.clone();
        move |window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(&vol_id, window, cx);
        }
    };
    let reset_vol_id = track_id.clone();
    let on_volume_reset = {
        let cb = callbacks.on_volume_change.clone();
        move |window: &mut gpui::Window, cx: &mut gpui::App| {
            cb(
                &(
                    reset_vol_id.clone(),
                    crate::components::timeline::timeline_state::volume::db_to_norm(0.0),
                ),
                window,
                cx,
            );
        }
    };
    let on_pan_change = callbacks.on_pan_change.clone();
    let on_pan_drag_start = callbacks.on_pan_drag_start.clone();
    let on_pan_drag_preview = callbacks.on_pan_drag_preview.clone();
    let on_pan_drag_commit = callbacks.on_pan_drag_commit.clone();
    let context_id = track_id.clone();
    let on_context = callbacks.on_context_menu.clone();
    let drag_track_id = track_id.clone();
    let drag_name = track.name.clone();
    let drag_color = track.color;
    let collapse_group_id = track_id.clone();
    let toggle_group_collapsed = callbacks.on_toggle_group_collapsed.clone();
    // The inline rename draws in place of this track's name only; every other
    // row keeps its plain name and the double-click that opens the editor.
    let rename = callbacks
        .rename
        .as_ref()
        .filter(|rename| rename.track_id == track.id)
        .cloned();
    let begin_rename = callbacks.on_begin_rename.clone();
    let rename_track_id = track_id.clone();

    let drag_item = TrackDragItem {
        track_id: drag_track_id,
        origin_index: index,
        name: drag_name,
        color: drag_color,
        is_group,
    };

    div()
        .flex()
        .flex_row()
        .w(px(HEADER_WIDTH))
        .h(px(row_height))
        // Clip to the row so a mid-resize frame can never paint controls
        // outside the row bounds; the adaptive layout keeps content within.
        .overflow_hidden()
        .relative()
        // Always opaque. The header column is the one region the arrangement
        // must never show through, and a state tint cannot carry that on its
        // own — see `header_tint`.
        .bg(header_base)
        .opacity(if is_dragging { 0.62 } else { 1.0 })
        .border_r(px(1.0))
        .border_color(Colors::border_strong())
        .border_b(px(1.0))
        .id(("track-header", id_num))
        .on_mouse_down(gpui::MouseButton::Left, on_select_root)
        .when_some(on_context, |this, cb| {
            this.on_mouse_down(gpui::MouseButton::Right, move |event, window, cx| {
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                cb(&(context_id.clone(), x, y), window, cx);
            })
        })
        // State tint over the opaque base.
        .when_some(header_tint, |header, tint| {
            header.child(div().absolute().inset_0().bg(tint))
        })
        .child(number_tab(
            track,
            index + 1,
            is_selected,
            row_height,
            id_num,
            drag_item.clone(),
        ))
        // One rail per enclosing folder, in its colour, outermost first.
        .children(folder.rails.iter().enumerate().map(|(level, color)| {
            let level = level.min(FOLDER_MAX_INDENT_LEVELS.saturating_sub(1));
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(FOLDER_RAIL_X + level as f32 * FOLDER_INDENT))
                .w(px(FOLDER_RAIL_W))
                .bg(*color)
        }))
        .child(
            div()
                .flex()
                .flex_col()
                // 6 + 24 + 4 + 22 + 6 = 62: both lines inside the default
                // 72 px row.
                .gap(px(space::TIGHT))
                // One line centres in a short row; two stack from the top of
                // a tall one, so a taller row grows the lane, not gaps.
                .when(!show_controls, |c| c.justify_center())
                .flex_1()
                .min_w_0()
                // Rows inside folders indent one step per level, past their
                // folders' rails.
                .pl(px(space::BASE + folder.indent()))
                .pr(px(space::SNUG))
                .py(px(space::SNUG))
                // Line 1: toggles, name, lane modes.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::SNUG))
                        .w_full()
                        .min_w(px(0.0))
                        .child(state_strip(
                            track,
                            id_num,
                            StateStripHandlers {
                                on_mute: Box::new(on_mute),
                                on_solo: Box::new(on_solo),
                                on_arm: Box::new(on_arm),
                                on_input: Box::new(on_input),
                            },
                        ))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(space::TIGHT))
                                .flex_1()
                                .min_w(px(0.0))
                                .h(px(size::DEFAULT))
                                .overflow_hidden()
                                .id(("track-drag-zone", id_num))
                                .on_drag(drag_item, move |drag, _offset, _window, cx| {
                                    cx.new(|_| TrackDragPreview {
                                        name: drag.name.clone(),
                                        color: drag.color,
                                    })
                                })
                                .when(is_group, |row| {
                                    row.child(
                                        div()
                                            .id(("folder-collapse", id_num))
                                            .flex()
                                            .flex_none()
                                            .items_center()
                                            .justify_center()
                                            .w(px(size::MICRO))
                                            .h(px(size::MICRO))
                                            .rounded(px(radius::CONTROL_SM))
                                            .cursor(gpui::CursorStyle::PointingHand)
                                            .hover(|style| style.bg(Colors::surface_hover()))
                                            .tooltip(fb_tooltip(if track.group_collapsed {
                                                "Open folder"
                                            } else {
                                                "Close folder"
                                            }))
                                            .on_mouse_down(
                                                gpui::MouseButton::Left,
                                                move |_event, window, cx| {
                                                    toggle_group_collapsed(
                                                        &collapse_group_id,
                                                        window,
                                                        cx,
                                                    );
                                                    cx.stop_propagation();
                                                },
                                            )
                                            .occlude()
                                            .child(
                                                svg()
                                                    .path(if track.group_collapsed {
                                                        assets::ICON_CHEVRON_RIGHT_PATH
                                                    } else {
                                                        assets::ICON_CHEVRON_DOWN_PATH
                                                    })
                                                    .w(px(GLYPH_SM))
                                                    .h(px(GLYPH_SM))
                                                    .text_color(Colors::text_secondary()),
                                            ),
                                    )
                                })
                                .child(match rename {
                                    // The field stops its own presses, so
                                    // editing never reselects the track or
                                    // starts a reorder drag.
                                    Some(rename) => div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .ml(px(-TRACK_NAME_FIELD_INSET))
                                        .child(text_field_with_callbacks_and_ime(
                                            &rename.input,
                                            rename.focused,
                                            rename.callbacks.clone(),
                                            rename.ime_target.clone(),
                                        ))
                                        .into_any_element(),
                                    None => div()
                                        .id(("track-name", id_num))
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .overflow_hidden()
                                        .truncate()
                                        .text_size(px(typography::UI_SM))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(Colors::text_primary())
                                        .child(track.name.clone())
                                        .on_click(move |event, window, cx| {
                                            if opens_track_rename(event, cx.has_active_drag()) {
                                                begin_rename(&rename_track_id, window, cx);
                                            }
                                        })
                                        .into_any_element(),
                                })
                                .when(is_group && track.group_collapsed, |row| {
                                    row.child(folder_member_badge(folder.members))
                                })
                                .when(!track.takes.is_empty(), |row| {
                                    row.child(takes_chip(
                                        track,
                                        id_num,
                                        callbacks.on_toggle_takes.clone(),
                                    ))
                                }),
                        )
                        .when_some(
                            callbacks
                                .on_open_instrument
                                .clone()
                                .filter(|_| track.track_type == TrackType::Instrument),
                            |row, on_open| {
                                row.child(instrument_header_button(track, id_num, on_open))
                            },
                        )
                        .child(mode_strip(
                            track,
                            is_automation,
                            id_num,
                            Box::new(on_automation),
                            on_freeze,
                        ))
                        .when_some(folder_menu, |row, (track_id, on_open)| {
                            row.child(folder_menu_button(id_num, track_id, on_open))
                        }),
                )
                // Line 2: level and pan, only where the row is tall enough
                // to hold it; a short row keeps line 1 alone.
                .when(show_controls, |col| {
                    col.child(
                        div()
                            .flex()
                            .flex_row()
                            // Presses on the bar and the knob stop here, short
                            // of the row: a drag on one of several selected
                            // tracks moves them all, which only works while
                            // the selection is still there. The drags
                            // themselves were registered on the controls.
                            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation()
                            })
                            .items_center()
                            .gap(px(space::SNUG))
                            .h(px(LEVEL_LINE_H))
                            .child(slim_horizontal_fader_with_drag_callbacks(
                                format!("track-vol-{}", track.id),
                                state.display_track_volume(track),
                                if is_selected {
                                    Colors::accent_primary()
                                } else {
                                    Colors::with_alpha(Colors::accent_primary(), 0.7)
                                },
                                Some(on_volume_drag_start),
                                Some(on_volume_drag_preview),
                                Some(on_volume_drag_commit),
                                Some(on_volume_reset),
                            ))
                            .child(readout(
                                DB_READOUT_W,
                                volume::format_db(state.display_track_volume(track)),
                                Some("dB"),
                            ))
                            .when(!track.dual_pan_active(), |row| {
                                row.child(div().flex_none().ml(px(space::TIGHT)).child(pan_knob(
                                    track,
                                    on_pan_drag_start,
                                    on_pan_drag_preview,
                                    on_pan_drag_commit,
                                    on_pan_change,
                                )))
                                .child(readout(
                                    PAN_READOUT_W,
                                    format_pan_label(track.pan),
                                    None,
                                ))
                            })
                            .when(track.dual_pan_active(), |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .ml(px(space::TIGHT))
                                        .child(dual_pan_badge(track)),
                                )
                            }),
                    )
                }),
        )
        // The meter: a full-height stereo pair in its own column at the
        // header's right edge, on the recessed plane, a hairline from the
        // controls. A slot only — the studio root paints the levels over it,
        // so a level change repaints the bars and not the arrangement.
        .child(
            div()
                .flex()
                .flex_none()
                .justify_center()
                .w(px(HEADER_METER_COLUMN_W))
                .h_full()
                .bg(Colors::surface_canvas())
                .border_l(px(1.0))
                .border_color(Colors::border_subtle())
                .child(track_meter_slot(meters, track.id.clone(), index)),
        )
}

#[cfg(test)]
mod rename_gesture_tests {
    use super::opens_track_rename;
    use gpui::{point, px, Modifiers};

    fn click(travel: f32, click_count: usize, down: Modifiers, up: Modifiers) -> gpui::ClickEvent {
        gpui::ClickEvent::Mouse(gpui::MouseClickEvent {
            down: gpui::MouseDownEvent {
                position: point(px(40.0), px(12.0)),
                modifiers: down,
                ..Default::default()
            },
            up: gpui::MouseUpEvent {
                position: point(px(40.0 + travel), px(12.0)),
                modifiers: up,
                click_count,
                ..Default::default()
            },
        })
    }

    fn plain() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn a_plain_stationary_double_click_opens_the_editor() {
        assert!(opens_track_rename(&click(0.0, 2, plain(), plain()), false));
        assert!(opens_track_rename(&click(2.0, 2, plain(), plain()), false));
    }

    #[test]
    fn a_single_click_only_selects() {
        assert!(!opens_track_rename(&click(0.0, 1, plain(), plain()), false));
    }

    /// Two quick reorder drags started on the name arrive as a double-click.
    #[test]
    fn drags_never_open_the_editor() {
        assert!(!opens_track_rename(
            &click(40.0, 2, plain(), plain()),
            false
        ));
        assert!(!opens_track_rename(&click(0.0, 2, plain(), plain()), true));
    }

    /// A modified double-click is the header's selection gesture twice over.
    #[test]
    fn a_modified_double_click_does_not_open_the_editor() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert!(!opens_track_rename(&click(0.0, 2, cmd, cmd), false));
        assert!(!opens_track_rename(&click(0.0, 2, shift, plain()), false));
        assert!(!opens_track_rename(&click(0.0, 2, plain(), alt), false));
    }
}
