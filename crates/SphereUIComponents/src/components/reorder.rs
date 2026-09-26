//! Shared drag-reorder affordances.
//!
//! Theme-tokened primitives and the pure drop rules used by every list that
//! supports drag reorder (FX/insert chains and send lists today), so the
//! reorder UX stays consistent and no surface hand-rolls its own drag chrome:
//!
//! * [`drag_handle`] — a compact grip the user presses to start a drag. The
//!   caller attaches the GPUI `.id(..).on_drag(payload, ..)` (the drag payload
//!   is list-specific), so only the handle initiates a reorder; the rest of a
//!   row's controls (buttons, context menu) keep their own hit-testing.
//! * [`drop_over_highlight`] — an accent top edge for a control that is a drop
//!   target as a whole (an add button).
//! * [`DropAnchor`] / [`DropSlot`] — where a drop lands, named by a
//!   neighbour's stable id instead of an index. Targets decide the anchor when
//!   they render; the commit resolves it against the live list, so a stale
//!   render (a detached window drawing from a pushed snapshot, a cached dock
//!   frame) can never land an item at the wrong place.
//! * [`DragRefusal`] — the "not allowed" cursor over a target that refuses.
//!
//! GPUI's drag machinery applies its own small click-vs-drag movement threshold
//! internally, so a press that does not move still registers as a click on the
//! handle.

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    div, px, App, CursorStyle, Div, InteractiveElement, ParentElement, SharedString, Stateful,
    StyleRefinement, Styled, Window,
};

use crate::components::panel::FxSlotDrag;
use crate::components::timeline::timeline_state::{
    is_insert_parameter_lane, AutomationLaneState, InsertSlotState, MasterBusState, TimelineState,
    TrackState, MASTER_TRACK_ID, MAX_INSERT_SLOTS,
};
use crate::theme::Colors;

/// Compact vertical grip (two columns × three dots) used as a drag handle.
/// Subtle by default and sized for DAW density. Returns a plain [`Div`] so the
/// caller can chain `.id(..)` + `.on_drag(..)` (+ any hover treatment) on it.
/// Vector dots — no asset, no icon font, no emoji (respects the icon rules).
pub fn drag_handle() -> Div {
    let dot = || {
        div()
            .w(px(2.0))
            .h(px(2.0))
            .rounded(px(crate::theme::radius::PILL))
            .bg(Colors::text_faint())
    };
    let column = || {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(dot())
            .child(dot())
            .child(dot())
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .w(px(12.0))
        .h(px(16.0))
        .gap(px(2.0))
        .child(column())
        .child(column())
}

/// Drop-position indicator styling for `.drag_over::<T>(drop_over_highlight)`
/// on a control that is a drop target as a whole: an accent top edge (on a
/// control that already has a border, the border turns accent).
pub fn drop_over_highlight(style: StyleRefinement) -> StyleRefinement {
    style
        .border_t(px(1.0))
        .border_color(Colors::accent_primary())
}

/// Where a dropped item lands, named by a neighbour's stable id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropAnchor {
    /// In front of this item.
    Before(String),
    /// Right after this item.
    After(String),
    /// At the end of the list.
    End,
}

impl DropAnchor {
    /// Whether the drop line belongs on the target's bottom edge: a row
    /// dragged down onto another lands below it.
    pub fn marks_bottom_edge(&self) -> bool {
        matches!(self, DropAnchor::After(_))
    }
}

/// A drop target's place in its list, captured when it renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropSlot {
    /// A row of the list.
    Row { id: String, index: usize },
    /// The end of the list. `last_id` is the row currently last, so dragging
    /// that row onto the end — a drop that changes nothing — is refused.
    End { last_id: Option<String> },
}

/// Where a row dragged within its own list lands on `slot`.
///
/// Dropping on a row takes that row's place, whichever way the drag went: a
/// row dragged down lands after its target, one dragged up lands before it.
/// `None` for a drop that would change nothing — the dragged row itself, or
/// the end of the list when it is already last — so the target shows no line
/// and refuses the drop instead of promising a move it does not make.
pub fn same_list_anchor(
    dragged_id: &str,
    dragged_index: usize,
    slot: &DropSlot,
) -> Option<DropAnchor> {
    match slot {
        DropSlot::Row { id, index } => {
            if id == dragged_id {
                None
            } else if dragged_index < *index {
                Some(DropAnchor::After(id.clone()))
            } else {
                Some(DropAnchor::Before(id.clone()))
            }
        }
        DropSlot::End { last_id } => {
            (last_id.as_deref() != Some(dragged_id)).then_some(DropAnchor::End)
        }
    }
}

/// Where an item from another list lands on `slot`: in the row's place,
/// pushing it down, or at the end.
pub fn foreign_anchor(slot: &DropSlot) -> DropAnchor {
    match slot {
        DropSlot::Row { id, .. } => DropAnchor::Before(id.clone()),
        DropSlot::End { .. } => DropAnchor::End,
    }
}

/// The gap (0..=len, between items, counted before the dragged item is taken
/// out) that `anchor` names in `order`. `None` when the row it names is no
/// longer in the list.
pub fn anchor_gap(order: &[String], anchor: &DropAnchor) -> Option<usize> {
    match anchor {
        DropAnchor::Before(id) => order.iter().position(|item| item == id),
        DropAnchor::After(id) => order.iter().position(|item| item == id).map(|i| i + 1),
        DropAnchor::End => Some(order.len()),
    }
}

/// The live `order` after moving `dragged` to `anchor`, never above `floor`
/// (the first index the list lets a dragged row occupy — see
/// `TrackState::fx_chain_floor`). `None` when the drop is refused: `dragged`
/// is not in the list, sits below the floor itself (it is the instrument), or
/// the anchor's row has gone. A result equal to `order` is a no-op.
pub fn reorder_to_anchor(
    order: &[String],
    dragged: &str,
    anchor: &DropAnchor,
    floor: usize,
) -> Option<Vec<String>> {
    let from = order.iter().position(|item| item == dragged)?;
    if from < floor {
        return None;
    }
    let gap = anchor_gap(order, anchor)?.max(floor);
    Some(TimelineState::reordered_insert_ids(order, dragged, gap))
}

/// How a drop target shows where a drop would land.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DropIndicator {
    /// A row that owns `gap` px of spacing below it: a 1px accent line in the
    /// spacing above the row, or below it for [`DropAnchor::After`]. Drawn in
    /// the spacing rows already have, so neither the row nor its neighbours
    /// move while a drag passes over them.
    Row { gap: f32 },
    /// A control with a border of its own: the top edge turns accent.
    Outline,
}

impl DropIndicator {
    /// The drag-over style for a drop landing at `anchor`.
    pub fn apply(self, style: StyleRefinement, anchor: &DropAnchor) -> StyleRefinement {
        match self {
            DropIndicator::Row { gap } => {
                let style = style.border_color(Colors::accent_primary());
                if anchor.marks_bottom_edge() {
                    style.border_b(px(1.0)).pb(px((gap - 1.0).max(0.0)))
                } else {
                    style.border_t(px(1.0)).mt(px(-1.0))
                }
            }
            DropIndicator::Outline => drop_over_highlight(style),
        }
    }
}

/// The cursor a slot drag shows wherever it could land. Slot rows use a
/// pointing hand, and the drag inherits the cursor of the row it started on.
pub const SLOT_DRAG_CURSOR: CursorStyle = CursorStyle::PointingHand;

/// What one pointer move over a drop target does to the drag cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragCursorChange {
    Keep,
    /// Show "not allowed": the pointer is over a target that refuses the drop.
    Refuse,
    /// Back to [`SLOT_DRAG_CURSOR`].
    Restore,
}

/// Pure rule behind [`DragRefusal::track`]. `current` is the target that last
/// refused, `target` the one reporting; `inside` is whether the pointer is in
/// it. Only the target that set "not allowed" puts the cursor back when the
/// pointer leaves it, so two targets never fight over the cursor.
pub fn drag_cursor_change(
    current: Option<&str>,
    target: &str,
    inside: bool,
    refused: bool,
) -> DragCursorChange {
    match (inside, refused) {
        (true, true) if current == Some(target) => DragCursorChange::Keep,
        (true, true) => DragCursorChange::Refuse,
        (true, false) if current.is_some() => DragCursorChange::Restore,
        (false, _) if current == Some(target) => DragCursorChange::Restore,
        _ => DragCursorChange::Keep,
    }
}

/// Which drop target last turned the drag cursor to "not allowed". One per
/// drag: every clone of the drag payload shares it.
#[derive(Clone, Default)]
pub struct DragRefusal(Rc<RefCell<Option<SharedString>>>);

impl DragRefusal {
    /// Forget any refusal. Called when a drag starts: the payload can outlive
    /// one drag (a cached frame keeps the element that owns it), and a stale
    /// entry would stop the next drag's refusal from showing.
    pub fn reset(&self) {
        self.0.borrow_mut().take();
    }

    /// Report the pointer's position against one target. Called from every
    /// target's `on_drag_move`, which GPUI runs for the whole drag whether or
    /// not the pointer is over that target.
    pub fn track(
        &self,
        target: &SharedString,
        inside: bool,
        refused: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let change = {
            let current = self.0.borrow();
            drag_cursor_change(current.as_deref(), target, inside, refused)
        };
        match change {
            DragCursorChange::Keep => {}
            DragCursorChange::Refuse => {
                *self.0.borrow_mut() = Some(target.clone());
                cx.set_active_drag_cursor_style(CursorStyle::OperationNotAllowed, window);
            }
            DragCursorChange::Restore => {
                *self.0.borrow_mut() = None;
                cx.set_active_drag_cursor_style(SLOT_DRAG_CURSOR, window);
            }
        }
    }
}

/// One insert drop, as the commit receives it. Resolved against the live
/// chains by `StudioLayout::commit_insert_drop`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertDrop {
    pub from_track: String,
    pub insert_id: String,
    pub to_track: String,
    pub anchor: DropAnchor,
    /// Alt was held on release: land a new instance of the same plug-in with
    /// the dragged one's state, and leave the dragged one where it is.
    pub copy: bool,
}

/// Whether a slot drag is a copy right now: Alt, alone, is held.
pub fn is_copy_drag(window: &Window) -> bool {
    let modifiers = window.modifiers();
    modifiers.alt && !modifiers.control && !modifiers.platform && !modifiers.shift
}

/// Commit callback for an insert drop. One completed drag = one undo entry.
pub type InsertDropCb = Arc<dyn Fn(&InsertDrop, &mut Window, &mut App) + 'static>;

impl FxSlotDrag {
    /// The drag payload for `slot` at chain index `source_index` on
    /// `track_id`. `lanes` is the owning track's automation (the master has
    /// none).
    pub fn for_slot(
        track_id: &str,
        slot: &InsertSlotState,
        source_index: usize,
        lanes: &[AutomationLaneState],
    ) -> Self {
        Self {
            track_id: track_id.to_string(),
            insert_id: slot.id.clone(),
            display_name: slot.display_name.clone(),
            source_index,
            movable: slot.plugin_is_instrument != Some(true),
            has_param_lanes: lanes
                .iter()
                .any(|lane| is_insert_parameter_lane(lane, &slot.id)),
            refusal: DragRefusal::default(),
        }
    }
}

/// What an insert-chain drop target knows about its own channel, captured when
/// it renders.
#[derive(Debug, Clone)]
pub struct InsertDropTarget {
    pub track_id: String,
    pub slot: DropSlot,
    /// Whether an effect from another channel may land here.
    pub accepts_foreign: bool,
    /// The master keeps no automation lanes, so it refuses an insert that
    /// has any.
    pub is_master: bool,
    /// Unique per target on its surface; files the refused cursor.
    pub key: SharedString,
}

impl InsertDropTarget {
    /// A target on `track`'s chain. `surface` keeps keys unique when one
    /// window shows the same chain twice (the Inspector beside the docked
    /// mixer).
    pub fn for_track(track: &TrackState, slot: DropSlot, surface: &str) -> Self {
        Self::new(&track.id, slot, track.accepts_moved_effect(), surface)
    }

    /// A target on the master's chain.
    pub fn for_master(master: &MasterBusState, slot: DropSlot, surface: &str) -> Self {
        Self::new(
            MASTER_TRACK_ID,
            slot,
            master.inserts.len() < MAX_INSERT_SLOTS,
            surface,
        )
    }

    fn new(track_id: &str, slot: DropSlot, accepts_foreign: bool, surface: &str) -> Self {
        let place = match &slot {
            DropSlot::Row { id, .. } => id.as_str(),
            DropSlot::End { .. } => "<end>",
        };
        Self {
            key: SharedString::from(format!("{surface}/{track_id}/{place}")),
            track_id: track_id.to_string(),
            slot,
            accepts_foreign,
            is_master: track_id == MASTER_TRACK_ID,
        }
    }

    /// The same target filed under another key — for a second drop target
    /// that stands for this one (an end strip and the add button below it).
    pub fn with_key_suffix(mut self, suffix: &str) -> Self {
        self.key = SharedString::from(format!("{}/{suffix}", self.key));
        self
    }

    /// Where `drag` lands on this target, or `None` when it is refused.
    ///
    /// Within one chain the [`same_list_anchor`] rule applies. From another
    /// chain the drop takes the row's place, and is refused when this channel
    /// cannot take an effect, when the insert is an instrument plug-in (its
    /// identity is positional and it drives MIDI routing), or when this is the
    /// master and the insert has plug-in parameter automation.
    ///
    /// A `copy` adds a slot instead of moving one, so it needs room on this
    /// chain even within the dragged insert's own, and never lands an
    /// instrument. It carries no automation lanes, so the master takes it
    /// either way. Dropped on the dragged row itself it lands right after it.
    pub fn anchor_for(&self, drag: &FxSlotDrag, copy: bool) -> Option<DropAnchor> {
        if copy {
            if !self.accepts_foreign || !drag.movable {
                return None;
            }
            if drag.track_id != self.track_id {
                return Some(foreign_anchor(&self.slot));
            }
            return match &self.slot {
                DropSlot::Row { id, .. } if *id == drag.insert_id => {
                    Some(DropAnchor::After(id.clone()))
                }
                DropSlot::End { .. } => Some(DropAnchor::End),
                slot => same_list_anchor(&drag.insert_id, drag.source_index, slot),
            };
        }
        if drag.track_id == self.track_id {
            return same_list_anchor(&drag.insert_id, drag.source_index, &self.slot);
        }
        if !self.accepts_foreign || !drag.movable || (self.is_master && drag.has_param_lanes) {
            return None;
        }
        Some(foreign_anchor(&self.slot))
    }
}

/// A slot drag payload: its drop targets report refusals through the cursor.
pub trait RefusableDrag: 'static {
    fn refusal(&self) -> &DragRefusal;
}

impl RefusableDrag for FxSlotDrag {
    fn refusal(&self) -> &DragRefusal {
        &self.refusal
    }
}

/// Make `element` a drop target for `T`: accepts a drag wherever `anchor_for`
/// finds a landing place, draws `indicator` on the edge that place is on,
/// shows "not allowed" over it when it refuses, and hands the drop to
/// `on_drop` as an anchor for the commit to resolve against live state.
/// `also_accept` answers `can_drop` for other payload types the element has
/// its own `on_drop` for (GPUI keeps a single predicate per element).
///
/// `anchor_for` is told whether the drag is a copy ([`is_copy_drag`]) as of
/// each question, so the line and the refusal follow Alt as it is pressed and
/// released mid-drag. A list that has no copy ignores it.
///
/// GPUI takes the drag before it asks `can_drop`, so a target that refuses
/// still eats the drop: rows have to cover their share of the spacing between
/// them rather than rely on a parent to catch what falls between.
pub fn slot_drop_target<T: RefusableDrag>(
    element: Stateful<Div>,
    key: SharedString,
    indicator: DropIndicator,
    anchor_for: impl Fn(&T, bool) -> Option<DropAnchor> + 'static,
    on_drop: impl Fn(&T, DropAnchor, &mut Window, &mut App) + 'static,
    also_accept: fn(&dyn Any) -> bool,
) -> Stateful<Div> {
    let anchor_for: Rc<dyn Fn(&T, bool) -> Option<DropAnchor>> = Rc::new(anchor_for);
    let over_anchor = anchor_for.clone();
    let move_anchor = anchor_for.clone();
    accept_slot_drops(element, anchor_for, on_drop, also_accept)
        .drag_over::<T>(move |style, drag, window, _cx| {
            match over_anchor(drag, is_copy_drag(window)) {
                Some(anchor) => indicator.apply(style, &anchor),
                None => style,
            }
        })
        .on_drag_move::<T>(move |event, window, cx| {
            let inside = event.bounds.contains(&event.event.position);
            let copy = is_copy_drag(window);
            let (refusal, refused) = {
                let drag = event.drag(cx);
                (drag.refusal().clone(), move_anchor(drag, copy).is_none())
            };
            refusal.track(&key, inside, refused, window, cx);
        })
}

/// The accepting half of a drop target: `can_drop` answers with `anchor_for`
/// (or `also_accept` for other payloads), and a drop that finds a landing
/// place goes to `on_drop`. No indicator and no cursor tracking.
fn accept_slot_drops<T: 'static>(
    element: Stateful<Div>,
    anchor_for: Rc<dyn Fn(&T, bool) -> Option<DropAnchor>>,
    on_drop: impl Fn(&T, DropAnchor, &mut Window, &mut App) + 'static,
    also_accept: fn(&dyn Any) -> bool,
) -> Stateful<Div> {
    let can_anchor = anchor_for.clone();
    element
        .can_drop(
            move |dragged, window, _cx| match dragged.downcast_ref::<T>() {
                Some(drag) => can_anchor(drag, is_copy_drag(window)).is_some(),
                None => also_accept(dragged),
            },
        )
        .on_drop::<T>(move |drag, window, cx| {
            if let Some(anchor) = anchor_for(drag, is_copy_drag(window)) {
                on_drop(drag, anchor, window, cx);
            }
        })
}

/// Make `element` an insert drop target — see [`slot_drop_target`] and
/// [`InsertDropTarget::anchor_for`].
pub fn insert_drop_target(
    element: Stateful<Div>,
    target: InsertDropTarget,
    indicator: DropIndicator,
    on_drop: InsertDropCb,
) -> Stateful<Div> {
    insert_drop_target_also(element, target, indicator, on_drop, |_| false)
}

/// [`insert_drop_target`] on an element that also takes other payloads
/// through its own `on_drop` listeners; `also_accept` answers for those.
pub fn insert_drop_target_also(
    element: Stateful<Div>,
    target: InsertDropTarget,
    indicator: DropIndicator,
    on_drop: InsertDropCb,
    also_accept: fn(&dyn Any) -> bool,
) -> Stateful<Div> {
    let target = Rc::new(target);
    let anchor_target = target.clone();
    slot_drop_target::<FxSlotDrag>(
        element,
        target.key.clone(),
        indicator,
        move |drag, copy| anchor_target.anchor_for(drag, copy),
        commit_insert_drop_to(target, on_drop),
        also_accept,
    )
}

/// Pass insert drops released over `element` to the row it sits on. For a
/// glyph button inside a row that is an [`insert_drop_target`] for `target`:
/// the glyph blocks the row's hitbox, so a drop released over it would never
/// reach the row. It lands exactly where the row says.
///
/// It draws no indicator and does not track the refusal cursor. The row's
/// bounds contain the glyph, so the row's own tracking already covers the
/// pointer here — one refusal state per row. A second tracker under the row's
/// key would report "pointer not inside" from every glyph the pointer is not
/// over, and put the cursor back while the pointer is still in the row.
pub fn insert_drop_forwarder(
    element: Stateful<Div>,
    target: InsertDropTarget,
    on_drop: InsertDropCb,
) -> Stateful<Div> {
    let target = Rc::new(target);
    let anchor_target = target.clone();
    accept_slot_drops::<FxSlotDrag>(
        element,
        Rc::new(move |drag, copy| anchor_target.anchor_for(drag, copy)),
        commit_insert_drop_to(target, on_drop),
        |_| false,
    )
}

/// Hand a drop on `target` to `on_drop` as an [`InsertDrop`].
fn commit_insert_drop_to(
    target: Rc<InsertDropTarget>,
    on_drop: InsertDropCb,
) -> impl Fn(&FxSlotDrag, DropAnchor, &mut Window, &mut App) + 'static {
    move |drag, anchor, window, cx| {
        on_drop(
            &InsertDrop {
                from_track: drag.track_id.clone(),
                insert_id: drag.insert_id.clone(),
                to_track: target.track_id.clone(),
                anchor,
                // Read in the same dispatch that found the anchor for it.
                copy: is_copy_drag(window),
            },
            window,
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn row(id: &str, index: usize) -> DropSlot {
        DropSlot::Row {
            id: id.to_string(),
            index,
        }
    }

    fn drag(track: &str, id: &str, index: usize) -> FxSlotDrag {
        FxSlotDrag {
            track_id: track.to_string(),
            insert_id: id.to_string(),
            display_name: id.to_string(),
            source_index: index,
            movable: true,
            has_param_lanes: false,
            refusal: DragRefusal::default(),
        }
    }

    fn target(track: &str, slot: DropSlot, accepts_foreign: bool) -> InsertDropTarget {
        InsertDropTarget::new(track, slot, accepts_foreign, "test")
    }

    /// Dropping a row on the row below it takes that row's place — the move
    /// that used to need a drop two rows further down.
    #[test]
    fn dropping_a_row_on_the_next_one_moves_it_down_one() {
        let order = ids(&["A", "B", "C"]);
        let anchor = same_list_anchor("A", 0, &row("B", 1)).expect("a real move");
        assert_eq!(anchor, DropAnchor::After("B".into()));
        assert!(
            anchor.marks_bottom_edge(),
            "moving down marks the bottom edge"
        );
        assert_eq!(
            reorder_to_anchor(&order, "A", &anchor, 0).unwrap(),
            ids(&["B", "A", "C"])
        );
    }

    #[test]
    fn dropping_a_row_on_the_previous_one_moves_it_up_one() {
        let order = ids(&["A", "B", "C"]);
        let anchor = same_list_anchor("C", 2, &row("B", 1)).expect("a real move");
        assert_eq!(anchor, DropAnchor::Before("B".into()));
        assert!(!anchor.marks_bottom_edge(), "moving up marks the top edge");
        assert_eq!(
            reorder_to_anchor(&order, "C", &anchor, 0).unwrap(),
            ids(&["A", "C", "B"])
        );
    }

    #[test]
    fn no_op_targets_are_refused() {
        // The dragged row itself.
        assert_eq!(same_list_anchor("A", 0, &row("A", 0)), None);
        // The end, when the dragged row is already last.
        let end = DropSlot::End {
            last_id: Some("C".into()),
        };
        assert_eq!(same_list_anchor("C", 2, &end), None);
        // The end is a real move for any other row.
        assert_eq!(same_list_anchor("A", 0, &end), Some(DropAnchor::End));
        assert_eq!(
            reorder_to_anchor(&ids(&["A", "B", "C"]), "A", &DropAnchor::End, 0).unwrap(),
            ids(&["B", "C", "A"])
        );
    }

    /// The anchor names a row, not an index, so a chain that changed after the
    /// target rendered (a stale detached-mixer snapshot) still lands the drop
    /// next to the row the user aimed at.
    #[test]
    fn anchors_resolve_against_the_live_order() {
        // Rendered as [A, B, C]; the user drags C up onto B: before B.
        let anchor = same_list_anchor("C", 2, &row("B", 1)).unwrap();
        // Meanwhile the live chain became [B, X, A, C].
        let live = ids(&["B", "X", "A", "C"]);
        assert_eq!(
            reorder_to_anchor(&live, "C", &anchor, 0).unwrap(),
            ids(&["C", "B", "X", "A"])
        );
        // A row that has gone refuses rather than guessing.
        let gone = DropAnchor::Before("Z".into());
        assert_eq!(reorder_to_anchor(&live, "C", &gone, 0), None);
    }

    #[test]
    fn the_floor_keeps_the_instrument_first() {
        let order = ids(&["VSTI", "A", "B"]);
        // Nothing lands in front of the instrument.
        assert_eq!(
            reorder_to_anchor(&order, "B", &DropAnchor::Before("VSTI".into()), 1).unwrap(),
            ids(&["VSTI", "B", "A"])
        );
        // And the instrument itself never moves.
        assert_eq!(reorder_to_anchor(&order, "VSTI", &DropAnchor::End, 1), None);
    }

    #[test]
    fn foreign_drops_take_the_rows_place_or_the_end() {
        let t = target("track-b", row("X", 1), true);
        assert_eq!(
            t.anchor_for(&drag("track-a", "A", 0), false),
            Some(DropAnchor::Before("X".into()))
        );
        let end = target("track-b", DropSlot::End { last_id: None }, true);
        assert_eq!(
            end.anchor_for(&drag("track-a", "A", 3), false),
            Some(DropAnchor::End)
        );
        assert_eq!(
            anchor_gap(&ids(&["W", "X"]), &DropAnchor::Before("X".into())),
            Some(1)
        );
    }

    #[test]
    fn foreign_drops_are_refused_where_the_insert_cannot_go() {
        let full = target("track-b", row("X", 1), false);
        assert_eq!(full.anchor_for(&drag("track-a", "A", 0), false), None);

        let open = target("track-b", row("X", 1), true);
        let mut instrument = drag("track-a", "A", 0);
        instrument.movable = false;
        assert_eq!(open.anchor_for(&instrument, false), None);

        let master = target(MASTER_TRACK_ID, DropSlot::End { last_id: None }, true);
        let mut automated = drag("track-a", "A", 0);
        assert_eq!(master.anchor_for(&automated, false), Some(DropAnchor::End));
        automated.has_param_lanes = true;
        assert_eq!(master.anchor_for(&automated, false), None);
    }

    /// Alt-drag lands a copy: on its own row too (right after it), on the end
    /// even when it is already last, and on the master with automation — the
    /// lanes stay with the original.
    #[test]
    fn a_copy_lands_where_a_move_would_change_nothing() {
        let own_row = target("track-a", row("A", 0), true);
        assert_eq!(own_row.anchor_for(&drag("track-a", "A", 0), false), None);
        assert_eq!(
            own_row.anchor_for(&drag("track-a", "A", 0), true),
            Some(DropAnchor::After("A".into()))
        );

        let end = target(
            "track-a",
            DropSlot::End {
                last_id: Some("A".into()),
            },
            true,
        );
        assert_eq!(
            end.anchor_for(&drag("track-a", "A", 0), true),
            Some(DropAnchor::End)
        );

        let master = target(MASTER_TRACK_ID, DropSlot::End { last_id: None }, true);
        let mut automated = drag("track-a", "A", 0);
        automated.has_param_lanes = true;
        assert_eq!(master.anchor_for(&automated, true), Some(DropAnchor::End));
    }

    /// A copy adds a slot, so a full chain refuses it even within the
    /// insert's own chain, and an instrument is never copied.
    #[test]
    fn a_copy_needs_room_and_an_effect() {
        let full_own = target("track-a", row("B", 1), false);
        assert_eq!(full_own.anchor_for(&drag("track-a", "A", 0), true), None);
        assert_eq!(
            full_own.anchor_for(&drag("track-a", "A", 0), false),
            Some(DropAnchor::After("B".into())),
            "a move within a full chain still reorders"
        );

        let open = target("track-b", row("X", 1), true);
        let mut instrument = drag("track-a", "A", 0);
        instrument.movable = false;
        assert_eq!(open.anchor_for(&instrument, true), None);
    }

    #[test]
    fn only_the_refusing_target_restores_the_cursor() {
        use DragCursorChange::*;
        // Entering a refusing target.
        assert_eq!(drag_cursor_change(None, "a", true, true), Refuse);
        // Staying in it.
        assert_eq!(drag_cursor_change(Some("a"), "a", true, true), Keep);
        // Leaving it.
        assert_eq!(drag_cursor_change(Some("a"), "a", false, true), Restore);
        // Another target the pointer is not over leaves it alone.
        assert_eq!(drag_cursor_change(Some("a"), "b", false, false), Keep);
        // Moving straight onto an accepting target restores it.
        assert_eq!(drag_cursor_change(Some("a"), "b", true, false), Restore);
        // Moving straight onto another refusing target takes it over.
        assert_eq!(drag_cursor_change(Some("a"), "b", true, true), Refuse);
        // Over an accepting target with nothing refused, nothing changes.
        assert_eq!(drag_cursor_change(None, "b", true, false), Keep);
    }

    /// The refused target after one pointer move, given every
    /// `(key, inside, refused)` report in GPUI's dispatch order (a row before
    /// the glyphs inside it).
    fn after_move(current: Option<&str>, reports: &[(&str, bool, bool)]) -> Option<String> {
        let mut current = current.map(str::to_string);
        for &(target, inside, refused) in reports {
            match drag_cursor_change(current.as_deref(), target, inside, refused) {
                DragCursorChange::Keep => {}
                DragCursorChange::Refuse => current = Some(target.to_string()),
                DragCursorChange::Restore => current = None,
            }
        }
        current
    }

    /// A refusing row keeps "not allowed" while the pointer is anywhere in
    /// it, over its glyphs too. Only the row reports: the glyphs that forward
    /// drops to it ([`insert_drop_forwarder`]) track nothing. Were they to
    /// report under the row's key, the ones the pointer is not over would
    /// restore the cursor on every move.
    #[test]
    fn a_refusing_row_keeps_not_allowed_over_its_glyphs() {
        let row = "mixer/track-a/fx1";
        // Pointer over the row's label, then over its bypass glyph: the row's
        // bounds contain both.
        let over_label = after_move(None, &[(row, true, true)]);
        assert_eq!(over_label.as_deref(), Some(row));
        let over_glyph = after_move(over_label.as_deref(), &[(row, true, true)]);
        assert_eq!(over_glyph.as_deref(), Some(row));
        // Leaving the row puts the cursor back.
        assert_eq!(
            after_move(over_glyph.as_deref(), &[(row, false, true)]),
            None
        );

        // The failure a tracking glyph caused: its own bounds miss the pointer.
        assert_eq!(
            after_move(None, &[(row, true, true), (row, false, true)]),
            None
        );
    }
}
