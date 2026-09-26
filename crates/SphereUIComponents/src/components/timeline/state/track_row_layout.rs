use std::collections::HashMap;

use super::*;

/// Default arrangement row height (px).
pub const DEFAULT_TRACK_HEIGHT: f32 = 72.0;

/// Legacy alias — prefer [`DEFAULT_TRACK_HEIGHT`] or per-track layout.
pub const TRACK_HEIGHT: f32 = DEFAULT_TRACK_HEIGHT;

pub const MAX_TRACK_HEIGHT: f32 = 320.0;

/// Smallest row height any track type may take. Every track type renders the
/// same header, so they share one minimum rather than three one-off values.
/// It fits the compact single-row header (name + mute/solo/arm/input/automation)
/// with no clipping or overlap; the full volume/pan/meter control row appears
/// once the row is tall enough (see [`TRACK_HEADER_CONTROLS_MIN_HEIGHT`]).
pub const MIN_TRACK_ROW_HEIGHT: f32 = 44.0;

/// At or above this row height the track header shows its full two-row layout
/// (name/buttons row + volume/pan/meter/dB row). Below it the header collapses
/// to the compact single-row layout so controls never spill outside the row.
/// The header's intrinsic two-row height equals [`DEFAULT_TRACK_HEIGHT`], so
/// the control row only appears at the default size or larger.
pub const TRACK_HEADER_CONTROLS_MIN_HEIGHT: f32 = 72.0;

pub const TRACK_HEIGHT_SMALL: f32 = 48.0;
pub const TRACK_HEIGHT_NORMAL: f32 = 72.0;
pub const TRACK_HEIGHT_LARGE: f32 = 120.0;
pub const TRACK_HEIGHT_HUGE: f32 = 180.0;

pub const TRACK_RESIZE_HANDLE_HITBOX: f32 = 5.0;

/// Fixed height (px) of a single expanded automation sub-lane row rendered
/// below its parent track. Compact DAW density; parent track height and lane
/// height are tracked independently.
pub const AUTOMATION_SUBLANE_HEIGHT: f32 = 58.0;

/// Fixed height (px) of the UI-only automation control row shown directly below
/// the parent track when automation is expanded. Not an audio track or envelope
/// lane — reconstructed from parent track automation state.
pub const AUTOMATION_CONTROL_LANE_HEIGHT: f32 = 32.0;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackViewLayout {
    /// Per-track row height overrides (px). Missing ids use [`DEFAULT_TRACK_HEIGHT`].
    heights: HashMap<TrackId, f32>,
}

impl TrackViewLayout {
    pub fn height_for(&self, track_id: &str) -> Option<f32> {
        self.heights.get(track_id).copied()
    }

    pub fn set_height(&mut self, track_id: impl Into<TrackId>, height: f32) {
        self.heights.insert(track_id.into(), height);
    }

    pub fn remove_track(&mut self, track_id: &str) {
        self.heights.remove(track_id);
    }

    pub fn clear(&mut self) {
        self.heights.clear();
    }

    pub fn retain_tracks<'a, I: Iterator<Item = &'a str>>(&mut self, live: I) {
        let live: std::collections::HashSet<&str> = live.collect();
        self.heights.retain(|id, _| live.contains(id.as_str()));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&TrackId, &f32)> {
        self.heights.iter()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRowLayoutEntry {
    pub track_id: TrackId,
    pub index: usize,
    pub y: f32,
    /// Height of the parent track row (clip lane), excluding any expanded
    /// automation sub-lanes below it.
    pub height: f32,
    /// Combined height (px) of the track's expanded automation sub-lanes,
    /// stacked directly below the parent row. `0.0` when the track's automation
    /// section is collapsed. The full block a track occupies vertically is
    /// `height + automation_height`.
    pub automation_height: f32,
}

impl TrackRowLayoutEntry {
    /// Total vertical space the track occupies: parent row + automation lanes.
    #[inline]
    pub fn block_height(&self) -> f32 {
        self.height + self.automation_height
    }
}

/// Vertical arrangement geometry for every track, in track order.
///
/// `rows` is 1:1 with `TimelineState::tracks` (`rows[i].index == i`), including
/// mixer-only channels, which are kept as zero-height entries so indices never
/// drift. Because each row starts where the previous one ends, `rows` is sorted
/// by `y`, and by `y + block_height()` — every content-y lookup here is a binary
/// search so a 2k-track arrangement costs the same per frame as a 20-track one.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackRowLayout {
    pub rows: Vec<TrackRowLayoutEntry>,
    pub total_height: f32,
    pub scroll_y: f32,
}

impl TrackRowLayout {
    pub fn build(state: &TimelineState) -> Self {
        let scroll_y = state.viewport.scroll_y;
        let mut y = 0.0_f32;
        let mut rows = Vec::with_capacity(state.tracks.len());
        let collapsed_groups = CollapsedGroups::of(&state.tracks);
        for (index, track) in state.tracks.iter().enumerate() {
            // Mixer-only channels (Bus/Return + VSTi multi-out children) live in
            // `state.tracks` for engine/mixer routing, but must NOT occupy
            // arrangement space. Keep them in the rows vector (1:1 with
            // `state.tracks`) but collapse them to zero height.
            let hidden_by_group = collapsed_groups.hides(track);
            let (height, automation_height) =
                if is_arrangement_hidden_track(track) || hidden_by_group {
                    (0.0, 0.0)
                } else {
                    (
                        state.track_row_height(track),
                        state.track_automation_height(track),
                    )
                };
            rows.push(TrackRowLayoutEntry {
                track_id: track.id.clone(),
                index,
                y,
                height,
                automation_height,
            });
            y += height + automation_height;
        }
        Self {
            rows,
            total_height: y,
            scroll_y,
        }
    }

    pub fn row_for_index(&self, index: usize) -> Option<&TrackRowLayoutEntry> {
        self.rows.get(index)
    }

    pub fn row_for_track(&self, track_id: &str) -> Option<&TrackRowLayoutEntry> {
        self.rows.iter().find(|row| row.track_id == track_id)
    }

    pub fn track_at_content_y(&self, content_y: f32) -> Option<&TrackRowLayoutEntry> {
        if content_y < 0.0 {
            return None;
        }
        // The whole vertical block (parent row + its automation sub-lanes)
        // resolves to the same track, so selecting / dragging over a sub-lane
        // still targets the owning track. Row bottoms are non-decreasing, so the
        // first row whose bottom is past `content_y` is the only candidate;
        // zero-height (mixer-only) rows have bottom == y and are skipped.
        let index = self
            .rows
            .partition_point(|row| row.y + row.block_height() <= content_y);
        self.rows.get(index).filter(|row| content_y >= row.y)
    }

    pub fn insert_index_at_content_y(&self, content_y: f32) -> usize {
        if self.rows.is_empty() {
            return 0;
        }
        let content_y = content_y.max(0.0);
        // Row midpoints are non-decreasing: insert before the first row whose
        // midpoint the pointer has not yet passed.
        self.rows
            .partition_point(|row| row.y + row.block_height() * 0.5 <= content_y)
    }
}

/// The arrangement's collapsed groups, and the one rule that hides a row
/// inside them: its own group is collapsed. The row layout and track zoom
/// share it, so zoom limits come from exactly the rows on screen.
struct CollapsedGroups<'a>(std::collections::HashSet<&'a str>);

impl<'a> CollapsedGroups<'a> {
    fn of(tracks: &'a [TrackState]) -> Self {
        Self(
            tracks
                .iter()
                .filter(|track| track.track_type == TrackType::Group && track.group_collapsed)
                .map(|track| track.id.as_str())
                .collect(),
        )
    }

    fn hides(&self, track: &TrackState) -> bool {
        track
            .parent_group_id
            .as_deref()
            .is_some_and(|group_id| self.0.contains(group_id))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackHeightResizeSession {
    pub anchor_track_id: String,
    pub track_ids: Vec<String>,
    pub start_heights: Vec<(String, f32)>,
    pub start_mouse_y: f32,
}

pub fn min_track_row_height(_track_type: TrackType) -> f32 {
    // All track types share one header layout, so one minimum keeps every
    // header (and its aligned timeline lane) from collapsing below a readable,
    // non-overlapping size. Kept type-parameterized for call-site stability.
    MIN_TRACK_ROW_HEIGHT
}

pub fn clamp_track_row_height(track_type: TrackType, height: f32) -> f32 {
    height.clamp(min_track_row_height(track_type), MAX_TRACK_HEIGHT)
}

pub fn preset_track_row_height(preset: TrackHeightPreset) -> f32 {
    match preset {
        TrackHeightPreset::Small => TRACK_HEIGHT_SMALL,
        TrackHeightPreset::Normal => TRACK_HEIGHT_NORMAL,
        TrackHeightPreset::Large => TRACK_HEIGHT_LARGE,
        TrackHeightPreset::Huge => TRACK_HEIGHT_HUGE,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackHeightPreset {
    Small,
    Normal,
    Large,
    Huge,
}

impl TimelineState {
    pub fn is_track_hidden_by_collapsed_group(&self, track: &TrackState) -> bool {
        track.parent_group_id.as_deref().is_some_and(|group_id| {
            self.tracks.iter().any(|group| {
                group.id == group_id
                    && group.track_type == TrackType::Group
                    && group.group_collapsed
            })
        })
    }

    pub fn track_row_height(&self, track: &TrackState) -> f32 {
        let raw = self
            .track_view_layout
            .height_for(&track.id)
            .unwrap_or(DEFAULT_TRACK_HEIGHT);
        clamp_track_row_height(track.track_type, raw)
    }

    pub fn track_row_height_for_id(&self, track_id: &str) -> f32 {
        self.tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| self.track_row_height(t))
            .unwrap_or(DEFAULT_TRACK_HEIGHT)
    }

    pub fn set_track_row_height(&mut self, track_id: &str, height: f32) -> bool {
        let Some(track) = self.tracks.iter().find(|t| t.id == track_id) else {
            return false;
        };
        let clamped = clamp_track_row_height(track.track_type, height);
        if (self.track_row_height(track) - clamped).abs() < 0.01 {
            return false;
        }
        self.track_view_layout.set_height(track_id, clamped);
        true
    }

    pub fn reset_track_row_height(&mut self, track_id: &str) -> bool {
        if self.track_view_layout.height_for(track_id).is_none() {
            return false;
        }
        self.track_view_layout.remove_track(track_id);
        true
    }

    pub fn reset_all_track_row_heights(&mut self) {
        self.track_view_layout.clear();
    }

    pub fn apply_track_row_heights(&mut self, heights: &[(String, f32)]) {
        for (track_id, height) in heights {
            let _ = self.set_track_row_height(track_id, *height);
        }
    }

    pub fn track_row_layout(&self) -> TrackRowLayout {
        TrackRowLayout::build(self)
    }

    /// True when the track's automation section is expanded (sub-lanes shown).
    /// Reuses [`TrackLaneMode::Automation`] as the expanded flag, so the header
    /// toggle drives it and the project's view section (v54) saves it.
    pub fn track_automation_expanded(&self, track: &TrackState) -> bool {
        track.lane_mode == TrackLaneMode::Automation
    }

    /// The automation lanes shown as sub-rows for a track: only when the
    /// automation section is expanded, and only lanes whose own show/hide flag
    /// is on. Order matches `track.automation_lanes`.
    pub fn visible_automation_lanes<'a>(
        &self,
        track: &'a TrackState,
    ) -> impl Iterator<Item = &'a AutomationLaneState> + 'a {
        let expanded = track.lane_mode == TrackLaneMode::Automation;
        track
            .automation_lanes
            .iter()
            .filter(move |lane| expanded && lane.visible)
    }

    /// Combined height of a track's expanded automation sub-lanes (0 collapsed).
    pub fn track_automation_height(&self, track: &TrackState) -> f32 {
        if !self.track_automation_expanded(track) {
            return 0.0;
        }
        let lane_count = self.visible_automation_lanes(track).count();
        AUTOMATION_CONTROL_LANE_HEIGHT + lane_count as f32 * AUTOMATION_SUBLANE_HEIGHT
    }

    /// Content-space y of the automation control row for `track_id`, or `None`
    /// when automation is collapsed.
    pub fn automation_control_lane_y(&self, track_id: &str) -> Option<f32> {
        let layout = self.track_row_layout();
        let row = layout.row_for_track(track_id)?;
        let track = self.find_track(track_id)?;
        if !self.track_automation_expanded(track) {
            return None;
        }
        Some(row.y + row.height)
    }

    /// Absolute content-space `(y, height)` of one automation sub-lane row, or
    /// `None` when the lane is not currently shown. Used by both the renderer
    /// (to place the row) and the interaction code (to map a window-y back to a
    /// normalized value within the correct sub-lane).
    pub fn automation_sublane_geometry(&self, track_id: &str, lane_id: &str) -> Option<(f32, f32)> {
        let layout = self.track_row_layout();
        let row = layout.row_for_track(track_id)?;
        let track = self.find_track(track_id)?;
        if !self.track_automation_expanded(track) {
            return None;
        }
        let mut y = row.y + row.height + AUTOMATION_CONTROL_LANE_HEIGHT;
        for lane in track.automation_lanes.iter().filter(|l| l.visible) {
            if lane.id == lane_id {
                return Some((y, AUTOMATION_SUBLANE_HEIGHT));
            }
            y += AUTOMATION_SUBLANE_HEIGHT;
        }
        None
    }

    pub fn total_track_rows_height(&self) -> f32 {
        self.track_row_layout().total_height
    }

    pub fn track_index_at_content_y(&self, content_y: f32) -> Option<usize> {
        self.track_row_layout()
            .track_at_content_y(content_y)
            .map(|row| row.index)
    }

    pub fn track_insert_index_at_content_y(&self, content_y: f32) -> usize {
        self.track_row_layout().insert_index_at_content_y(content_y)
    }

    pub fn track_index_at_y(&self, viewport_y: f32) -> Option<usize> {
        let content_y = viewport_y + self.viewport.scroll_y;
        self.track_index_at_content_y(content_y)
    }

    pub fn track_insert_index_at_y(&self, viewport_y: f32) -> usize {
        let content_y = (viewport_y + self.viewport.scroll_y).max(0.0);
        self.track_insert_index_at_content_y(content_y)
    }

    pub fn lane_y_to_track_id(&self, viewport_y: f32) -> Option<TrackId> {
        let content_y = viewport_y + self.viewport.scroll_y;
        self.track_row_layout()
            .track_at_content_y(content_y)
            .map(|row| row.track_id.clone())
    }

    pub fn track_height_resize_targets(
        &self,
        anchor_track_id: &str,
        shift: bool,
        alt: bool,
    ) -> Vec<String> {
        if alt {
            return self.tracks.iter().map(|t| t.id.clone()).collect();
        }
        if shift {
            let mut ids = self
                .arrangement_range
                .as_ref()
                .map(|range| range.track_ids.clone())
                .unwrap_or_default();
            if ids.is_empty() {
                if let Some(id) = &self.selection.selected_track_id {
                    ids.push(id.clone());
                }
            }
            if ids.is_empty() {
                ids.push(anchor_track_id.to_string());
            }
            return ids;
        }
        vec![anchor_track_id.to_string()]
    }

    pub fn arm_track_height_resize(
        &mut self,
        anchor_track_id: &str,
        start_mouse_y: f32,
        shift: bool,
        alt: bool,
    ) {
        self.track_height_resize_arm =
            Some((anchor_track_id.to_string(), start_mouse_y, shift, alt));
    }

    pub fn clear_track_height_resize_arm(&mut self) {
        self.track_height_resize_arm = None;
    }

    pub fn ensure_track_height_resize_from_arm(&mut self, mouse_y: f32) -> bool {
        if self.track_height_resize.is_some() {
            return true;
        }
        let Some((anchor, start_y, shift, alt)) = self.track_height_resize_arm.clone() else {
            return false;
        };
        self.track_height_resize_arm = None;
        self.begin_track_height_resize(&anchor, start_y, shift, alt);
        self.update_track_height_resize(mouse_y)
    }

    pub fn begin_track_height_resize(
        &mut self,
        anchor_track_id: &str,
        start_mouse_y: f32,
        shift: bool,
        alt: bool,
    ) -> bool {
        let track_ids = self.track_height_resize_targets(anchor_track_id, shift, alt);
        if track_ids.is_empty() {
            return false;
        }
        let start_heights = track_ids
            .iter()
            .filter_map(|id| {
                self.tracks
                    .iter()
                    .find(|t| t.id == *id)
                    .map(|t| (id.clone(), self.track_row_height(t)))
            })
            .collect::<Vec<_>>();
        if start_heights.is_empty() {
            return false;
        }
        self.track_height_resize = Some(TrackHeightResizeSession {
            anchor_track_id: anchor_track_id.to_string(),
            track_ids,
            start_heights,
            start_mouse_y,
        });
        true
    }

    pub fn update_track_height_resize(&mut self, mouse_y: f32) -> bool {
        let Some(session) = self.track_height_resize.clone() else {
            return false;
        };
        let delta = mouse_y - session.start_mouse_y;
        let mut changed = false;
        for (track_id, start_h) in &session.start_heights {
            if self.set_track_row_height(track_id, start_h + delta) {
                changed = true;
            }
        }
        changed
    }

    pub fn cancel_track_height_resize(&mut self) -> bool {
        self.track_height_resize_arm = None;
        let Some(session) = self.track_height_resize.take() else {
            return false;
        };
        for (track_id, height) in session.start_heights {
            if (height - DEFAULT_TRACK_HEIGHT).abs() < 0.01 {
                self.track_view_layout.remove_track(&track_id);
            } else {
                self.track_view_layout.set_height(track_id, height);
            }
        }
        true
    }

    pub fn finish_track_height_resize(
        &mut self,
    ) -> Option<(Vec<(String, f32)>, Vec<(String, f32)>)> {
        let session = self.track_height_resize.take()?;
        let prev = session.start_heights;
        let next = session
            .track_ids
            .iter()
            .filter_map(|id| {
                self.tracks
                    .iter()
                    .find(|t| t.id == *id)
                    .map(|t| (id.clone(), self.track_row_height(t)))
            })
            .collect::<Vec<_>>();
        let changed = prev
            .iter()
            .zip(next.iter())
            .any(|((id_a, h_a), (id_b, h_b))| id_a == id_b && (h_a - h_b).abs() >= 0.01);
        if changed {
            Some((prev, next))
        } else {
            None
        }
    }

    pub fn prune_track_view_layout(&mut self) {
        let ids = self.tracks.iter().map(|t| t.id.as_str());
        self.track_view_layout.retain_tracks(ids);
    }
}

/// Wheel silence that ends a track-zoom burst. The next tick then scales from
/// the heights as they are, not from the previous burst's snapshot.
pub const TRACK_ZOOM_BURST_IDLE: std::time::Duration = std::time::Duration::from_millis(300);

/// One arrangement track's height when a track-zoom burst started.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackZoomBase {
    pub track_id: TrackId,
    pub track_type: TrackType,
    pub height: f32,
    /// The row is on screen: not inside a collapsed group. Only these rows
    /// set the burst's limits and decide whether a tick changed anything;
    /// hidden rows follow the zoom, each clamped on its own.
    pub visible: bool,
}

/// A burst of Ctrl/Cmd+Alt+wheel ticks zooming every arrangement track's
/// height. View gesture state held by the `Timeline` view, not by
/// `TimelineState`, which is cloned and compared as document state.
///
/// Heights are always recomputed from `base` × `factor`, never compounded
/// tick by tick, so zooming into a limit and back within one burst restores
/// every custom height exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackHeightZoomSession {
    base: Vec<TrackZoomBase>,
    /// Zoom accumulated over the burst, relative to `base`.
    factor: f32,
    /// `factor` is held inside the range where at least one visible row can
    /// still change, so reversing direction after the rows on screen hit a
    /// limit reacts at once. A hidden row cannot hold the factor past that.
    factor_min: f32,
    factor_max: f32,
    /// Effective heights the last tick left, in `base` order. Any other writer
    /// (row drag, reset, undo, project load) makes the live heights differ,
    /// which restarts the burst instead of overwriting that change.
    applied: Vec<f32>,
    last_tick: std::time::Instant,
}

impl TrackHeightZoomSession {
    fn start(base: Vec<TrackZoomBase>, now: std::time::Instant) -> Option<Self> {
        if base.is_empty() {
            return None;
        }
        // With no row on screen the range stays [1, 1] and a tick does nothing.
        let (mut factor_min, mut factor_max) = (1.0_f32, 1.0_f32);
        for entry in base.iter().filter(|entry| entry.visible) {
            factor_min = factor_min.min(min_track_row_height(entry.track_type) / entry.height);
            factor_max = factor_max.max(MAX_TRACK_HEIGHT / entry.height);
        }
        let applied = base.iter().map(|entry| entry.height).collect();
        Some(Self {
            base,
            factor: 1.0,
            factor_min,
            factor_max,
            applied,
            last_tick: now,
        })
    }
}

/// What one track-zoom tick did, so the caller can apply its side effects.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackZoomTick {
    /// Scroll y keeping the anchored track under the pointer, clamped to the
    /// new content height. `None` when no visible row changed height.
    pub scroll_y: Option<f32>,
    /// A visible row changed: mark the project view-dirty. Every such tick
    /// marks it, so a save that lands mid-burst leaves the rest of the burst
    /// unsaved rather than lost. Track zoom is a view change, so it never
    /// records an undo entry and never marks the engine dirty.
    pub mark_view_dirty: bool,
}

/// Where the zoom anchor sits inside its track's block.
#[derive(Debug, Clone, Copy)]
enum TrackZoomAnchorOffset {
    /// A fraction of the clip row, which scales.
    ClipRow(f32),
    /// Pixels below the clip row, inside the automation sub-lanes, which do not.
    Automation(f32),
}

/// Height a track takes at `factor` of its burst-start height, or `None` for
/// the default height, so the override is removed and saves stay byte-stable.
fn zoomed_track_height(base: &TrackZoomBase, factor: f32) -> Option<f32> {
    let scaled = base.height * factor;
    // Within half a pixel of where it started, a row keeps its exact height,
    // so zooming back to factor 1 restores fractional custom heights.
    let height = if (scaled - base.height).abs() < 0.5 {
        base.height
    } else {
        clamp_track_row_height(base.track_type, scaled.round())
    };
    ((height - DEFAULT_TRACK_HEIGHT).abs() > 0.5).then_some(height)
}

impl TimelineState {
    /// Every arrangement track's current height, in track order. Mixer-only
    /// channels take no arrangement space and are left out; children of a
    /// collapsed group stay in, marked not visible, so they still match their
    /// siblings once the group expands.
    fn track_zoom_base(&self) -> Vec<TrackZoomBase> {
        let collapsed_groups = CollapsedGroups::of(&self.tracks);
        self.tracks
            .iter()
            .filter(|track| !is_arrangement_hidden_track(track))
            .map(|track| TrackZoomBase {
                track_id: track.id.clone(),
                track_type: track.track_type,
                height: self.track_row_height(track),
                visible: !collapsed_groups.hides(track),
            })
            .collect()
    }

    /// True while `session` still describes the arrangement: its last tick
    /// was recent, the arrangement tracks and the rows on screen are the same,
    /// and every height is still what that tick wrote.
    fn track_zoom_session_is_live(
        &self,
        session: &TrackHeightZoomSession,
        now: std::time::Instant,
    ) -> bool {
        if now.saturating_duration_since(session.last_tick) > TRACK_ZOOM_BURST_IDLE {
            return false;
        }
        let collapsed_groups = CollapsedGroups::of(&self.tracks);
        let mut live = self
            .tracks
            .iter()
            .filter(|track| !is_arrangement_hidden_track(track));
        for (base, applied) in session.base.iter().zip(&session.applied) {
            let Some(track) = live.next() else {
                return false;
            };
            if track.id != base.track_id
                || collapsed_groups.hides(track) == base.visible
                || (self.track_row_height(track) - applied).abs() >= 0.01
            {
                return false;
            }
        }
        live.next().is_none()
    }

    /// One Ctrl/Cmd+Alt+wheel tick: scale every arrangement track's height by
    /// `tick_factor` and keep the track under `anchor_viewport_y` in place.
    ///
    /// `anchor_viewport_y` is in track-area viewport space (the transform the
    /// arrangement hit-tests with) and `viewport_height` is the visible track
    /// area height, used to clamp the returned scroll.
    pub fn track_zoom_tick(
        &mut self,
        session: &mut Option<TrackHeightZoomSession>,
        tick_factor: f32,
        anchor_viewport_y: f32,
        viewport_height: f32,
        now: std::time::Instant,
    ) -> TrackZoomTick {
        // A row-resize drag owns the heights until it ends.
        if self.track_height_resize.is_some() || self.track_height_resize_arm.is_some() {
            *session = None;
            return TrackZoomTick::default();
        }
        if !tick_factor.is_finite() || tick_factor <= 0.0 {
            return TrackZoomTick::default();
        }
        if !session
            .as_ref()
            .is_some_and(|live| self.track_zoom_session_is_live(live, now))
        {
            *session = TrackHeightZoomSession::start(self.track_zoom_base(), now);
        }
        let Some(active) = session.as_mut() else {
            return TrackZoomTick::default();
        };
        active.last_tick = now;
        active.factor = (active.factor * tick_factor).clamp(active.factor_min, active.factor_max);
        let scroll_y = self.zoom_track_heights(
            &active.base,
            active.factor,
            anchor_viewport_y,
            viewport_height,
        );
        let factor = active.factor;
        active.applied.clear();
        active.applied.extend(
            active
                .base
                .iter()
                .map(|entry| zoomed_track_height(entry, factor).unwrap_or(DEFAULT_TRACK_HEIGHT)),
        );
        TrackZoomTick {
            scroll_y,
            mark_view_dirty: scroll_y.is_some(),
        }
    }

    /// Set every track in `base` to its height at `factor`, and return the
    /// scroll y that keeps the anchored track at `anchor_viewport_y`, clamped
    /// to `[0, total - viewport_height]`. `None` when no visible row changed:
    /// a hidden row takes no space, so it moves nothing on screen.
    ///
    /// Rows are 1:1 with `tracks` by index, so the anchor is a row index plus
    /// a fraction of its clip row; a pointer in the automation sub-lanes keeps
    /// its pixel offset, since those rows do not scale. With the pointer below
    /// the last row, the row at the top of the view anchors instead.
    pub fn zoom_track_heights(
        &mut self,
        base: &[TrackZoomBase],
        factor: f32,
        anchor_viewport_y: f32,
        viewport_height: f32,
    ) -> Option<f32> {
        let scroll_y = self.viewport.scroll_y;
        let before = self.track_row_layout();
        let anchor = [anchor_viewport_y.max(0.0), 0.0]
            .into_iter()
            .find_map(|viewport_y| {
                let content_y = viewport_y + scroll_y;
                let row = before.track_at_content_y(content_y)?;
                let offset = content_y - row.y;
                let within = if offset < row.height {
                    TrackZoomAnchorOffset::ClipRow(offset / row.height)
                } else {
                    TrackZoomAnchorOffset::Automation(offset - row.height)
                };
                Some((viewport_y, row.index, within))
            });

        let mut changed = false;
        for entry in base {
            let old = self
                .track_view_layout
                .height_for(&entry.track_id)
                .map_or(DEFAULT_TRACK_HEIGHT, |h| {
                    clamp_track_row_height(entry.track_type, h)
                });
            let next = zoomed_track_height(entry, factor);
            match next {
                Some(height) => self
                    .track_view_layout
                    .set_height(entry.track_id.clone(), height),
                None => self.track_view_layout.remove_track(&entry.track_id),
            }
            changed |= entry.visible && (next.unwrap_or(DEFAULT_TRACK_HEIGHT) - old).abs() >= 0.01;
        }
        if !changed {
            return None;
        }

        let after = self.track_row_layout();
        let max_scroll_y = (after.total_height - viewport_height).max(0.0);
        let next_scroll_y = anchor
            .and_then(|(viewport_y, index, within)| {
                let row = after.row_for_index(index)?;
                let content_y = match within {
                    TrackZoomAnchorOffset::ClipRow(fraction) => row.y + fraction * row.height,
                    TrackZoomAnchorOffset::Automation(offset) => row.y + row.height + offset,
                };
                Some(content_y - viewport_y)
            })
            .unwrap_or(scroll_y);
        Some(next_scroll_y.clamp(0.0, max_scroll_y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{CreateTrackOptions, TrackType};

    fn sample_state(track_types: &[TrackType]) -> TimelineState {
        let mut state = TimelineState::default();
        for (i, ty) in track_types.iter().enumerate() {
            state.create_track(CreateTrackOptions {
                name: format!("Track {i}"),
                track_type: *ty,
                color: crate::theme::Colors::track_color_for_index(i),
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
        }
        state
    }

    /// Reference implementation of the pre-binary-search lookups, kept in the
    /// tests so the O(log n) versions are checked against the linear semantics
    /// they replaced rather than against hand-picked expectations.
    fn linear_track_at_content_y(
        layout: &TrackRowLayout,
        content_y: f32,
    ) -> Option<&TrackRowLayoutEntry> {
        if content_y < 0.0 {
            return None;
        }
        layout
            .rows
            .iter()
            .find(|row| content_y >= row.y && content_y < row.y + row.block_height())
    }

    fn linear_insert_index_at_content_y(layout: &TrackRowLayout, content_y: f32) -> usize {
        if layout.rows.is_empty() {
            return 0;
        }
        let content_y = content_y.max(0.0);
        layout
            .rows
            .iter()
            .position(|row| content_y < row.y + row.block_height() * 0.5)
            .unwrap_or(layout.rows.len())
    }

    fn linear_visible_range(
        layout: &TrackRowLayout,
        scroll_y: f32,
        viewport_height: f32,
        overscan: usize,
    ) -> (usize, usize) {
        let count = layout.rows.len();
        let start = layout
            .rows
            .iter()
            .position(|row| row.y + row.block_height() > scroll_y)
            .unwrap_or(count)
            .saturating_sub(overscan);
        let end = layout
            .rows
            .iter()
            .position(|row| row.y >= scroll_y + viewport_height)
            .unwrap_or(count)
            .saturating_add(overscan)
            .min(count);
        (start, end)
    }

    /// A 2k-channel session shaped like the reported one: audio/MIDI arrangement
    /// rows interleaved with mixer-only channels (zero-height rows) and some
    /// resized rows, so the monotonicity the binary searches rely on is
    /// exercised against ties and varying heights.
    fn dense_session(track_count: usize) -> TimelineState {
        let mut state = TimelineState::default();
        for index in 0..track_count {
            let track_type = match index % 4 {
                0 => TrackType::Audio,
                1 => TrackType::Midi,
                2 => TrackType::Bus, // mixer-only: zero-height arrangement row
                _ => TrackType::Instrument,
            };
            state.create_track(CreateTrackOptions {
                name: format!("Track {index}"),
                track_type,
                color: crate::theme::Colors::track_color_for_index(index),
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
        }
        // Vary a few row heights so rows are not a uniform stride.
        for index in (0..track_count).step_by(7) {
            let id = state.tracks[index].id.clone();
            state.set_track_row_height(&id, TRACK_HEIGHT_LARGE);
        }
        state
    }

    #[test]
    fn binary_search_lookups_match_the_linear_scan_they_replaced() {
        let state = dense_session(2_000);
        let layout = state.track_row_layout();
        assert!(layout.total_height > 0.0);

        // Row tops and bottoms must be non-decreasing — the invariant every
        // binary search here depends on.
        for pair in layout.rows.windows(2) {
            assert!(pair[1].y >= pair[0].y);
            assert_eq!(pair[1].y, pair[0].y + pair[0].block_height());
        }

        let probes = [
            -10.0,
            0.0,
            1.0,
            layout.total_height * 0.25,
            layout.total_height * 0.5,
            layout.total_height - 1.0,
            layout.total_height,
            layout.total_height + 500.0,
        ];
        for content_y in probes {
            assert_eq!(
                layout.track_at_content_y(content_y).map(|row| row.index),
                linear_track_at_content_y(&layout, content_y).map(|row| row.index),
                "track_at_content_y diverged at {content_y}"
            );
            assert_eq!(
                layout.insert_index_at_content_y(content_y),
                linear_insert_index_at_content_y(&layout, content_y),
                "insert_index_at_content_y diverged at {content_y}"
            );
        }

        // Every row boundary, not just sampled points.
        for row in &layout.rows {
            for probe in [row.y, row.y + row.block_height() * 0.5] {
                assert_eq!(
                    layout.track_at_content_y(probe).map(|r| r.index),
                    linear_track_at_content_y(&layout, probe).map(|r| r.index),
                    "track_at_content_y diverged on row {} at {probe}",
                    row.index
                );
            }
        }
    }

    #[test]
    fn visible_row_range_matches_the_linear_scan_and_stays_bounded() {
        use crate::components::timeline::track_resize::visible_track_row_range;

        let state = dense_session(2_000);
        let layout = state.track_row_layout();
        let viewport_height = 520.0;

        for scroll_y in [
            0.0,
            10.0,
            layout.total_height * 0.5,
            layout.total_height - viewport_height,
            layout.total_height + 1_000.0,
        ] {
            let (start, end, top_spacer, bottom_spacer) =
                visible_track_row_range(&layout, scroll_y, viewport_height, 2);
            assert_eq!(
                (start, end),
                linear_visible_range(&layout, scroll_y, viewport_height, 2),
                "visible range diverged at scroll_y={scroll_y}"
            );
            assert!(start <= end);
            assert!(top_spacer >= 0.0 && bottom_spacer >= 0.0);
            // Virtualization budget: a 2k-track arrangement must never hand the
            // renderer more rows than a screenful plus overscan.
            assert!(
                end - start <= 24,
                "visible row window grew to {} rows at scroll_y={scroll_y}",
                end - start
            );
        }
    }

    #[test]
    fn track_row_layout_uses_default_height() {
        let state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let layout = state.track_row_layout();
        assert_eq!(layout.rows.len(), 2);
        assert_eq!(layout.rows[0].height, DEFAULT_TRACK_HEIGHT);
        assert_eq!(layout.rows[1].y, DEFAULT_TRACK_HEIGHT);
        assert_eq!(layout.total_height, DEFAULT_TRACK_HEIGHT * 2.0);
    }

    #[test]
    fn track_row_layout_respects_per_track_override() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let track_id = state.tracks[0].id.clone();
        state.track_view_layout.set_height(&track_id, 120.0);
        let layout = state.track_row_layout();
        assert_eq!(layout.rows[0].height, 120.0);
        assert_eq!(layout.rows[1].y, 120.0);
    }

    #[test]
    fn clamp_track_row_height_enforces_shared_minimum() {
        // Every track type clamps to the same shared minimum.
        for ty in [
            TrackType::Audio,
            TrackType::Midi,
            TrackType::Instrument,
            TrackType::Bus,
            TrackType::Return,
            TrackType::Group,
            TrackType::Master,
        ] {
            assert_eq!(clamp_track_row_height(ty, 10.0), MIN_TRACK_ROW_HEIGHT);
        }
        assert_eq!(
            clamp_track_row_height(TrackType::Audio, 500.0),
            MAX_TRACK_HEIGHT
        );
    }

    #[test]
    fn resize_session_restores_on_cancel() {
        let mut state = sample_state(&[TrackType::Audio]);
        let track_id = state.tracks[0].id.clone();
        state.begin_track_height_resize(&track_id, 100.0, false, false);
        state.update_track_height_resize(130.0);
        assert!(state.track_row_height(&state.tracks[0]) > DEFAULT_TRACK_HEIGHT);
        state.cancel_track_height_resize();
        assert_eq!(
            state.track_row_height(&state.tracks[0]),
            DEFAULT_TRACK_HEIGHT
        );
    }

    #[test]
    fn alt_resize_targets_all_tracks() {
        let state = sample_state(&[TrackType::Audio, TrackType::Midi, TrackType::Bus]);
        let anchor = state.tracks[0].id.clone();
        let ids = state.track_height_resize_targets(&anchor, false, true);
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn shift_resize_uses_selection_when_no_range() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        state.selection.selected_track_id = Some(state.tracks[1].id.clone());
        let anchor = state.tracks[0].id.clone();
        let ids = state.track_height_resize_targets(&anchor, true, false);
        assert_eq!(ids, vec![state.tracks[1].id.clone()]);
    }

    #[test]
    fn collapsed_group_hides_children_and_restores_them() {
        let mut state = sample_state(&[
            TrackType::Group,
            TrackType::Audio,
            TrackType::Audio,
            TrackType::Midi,
        ]);
        let group_id = state.tracks[0].id.clone();
        let first_child_id = state.tracks[1].id.clone();
        let second_child_id = state.tracks[2].id.clone();
        assert!(state.assign_track_to_group(&first_child_id, &group_id));
        assert!(state.assign_track_to_group(&second_child_id, &group_id));

        let expanded_height = state.total_track_rows_height();
        state.select_track(&first_child_id);
        assert_eq!(state.toggle_group_collapsed(&group_id), Some(true));
        let collapsed = state.track_row_layout();

        assert_eq!(
            collapsed.row_for_track(&first_child_id).unwrap().height,
            0.0
        );
        assert_eq!(
            collapsed.row_for_track(&second_child_id).unwrap().height,
            0.0
        );
        assert_eq!(
            collapsed.total_height,
            expanded_height - DEFAULT_TRACK_HEIGHT * 2.0
        );
        assert_eq!(
            state.selection.selected_track_id.as_deref(),
            Some(group_id.as_str())
        );

        assert_eq!(state.toggle_group_collapsed(&group_id), Some(false));
        assert_eq!(state.total_track_rows_height(), expanded_height);
    }

    use std::time::{Duration, Instant};

    /// Tall enough that no test below scrolls unless it means to.
    const ZOOM_VIEWPORT: f32 = 2_000.0;

    fn heights(state: &TimelineState) -> Vec<f32> {
        state
            .tracks
            .iter()
            .map(|track| state.track_row_height(track))
            .collect()
    }

    fn zoom_tick(
        state: &mut TimelineState,
        session: &mut Option<TrackHeightZoomSession>,
        factor: f32,
        anchor_viewport_y: f32,
        viewport_height: f32,
        now: Instant,
    ) -> TrackZoomTick {
        state.track_zoom_tick(session, factor, anchor_viewport_y, viewport_height, now)
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-3, "{actual} != {expected}");
    }

    /// Content y the pointer resolves to, as `(track index, offset into block)`.
    fn anchor_at(state: &TimelineState, viewport_y: f32) -> (usize, f32) {
        let layout = state.track_row_layout();
        let content_y = viewport_y + state.viewport.scroll_y;
        let row = layout
            .track_at_content_y(content_y)
            .expect("pointer over a track");
        (row.index, content_y - row.y)
    }

    /// Apply a zoom through the pure state function the way the wheel handler
    /// does: the returned scroll is written back.
    fn zoom_and_scroll(
        state: &mut TimelineState,
        factor: f32,
        anchor_viewport_y: f32,
        viewport_height: f32,
    ) -> Option<f32> {
        let base = state.track_zoom_base();
        let scroll_y = state.zoom_track_heights(&base, factor, anchor_viewport_y, viewport_height);
        if let Some(scroll_y) = scroll_y {
            state.viewport.scroll_y = scroll_y;
        }
        scroll_y
    }

    #[test]
    fn track_zoom_scales_every_arrangement_row_in_proportion() {
        let mut state = sample_state(&[
            TrackType::Audio,
            TrackType::Midi,
            TrackType::Instrument,
            TrackType::Bus,
            TrackType::Return,
            TrackType::Audio,
        ]);
        let ids: Vec<String> = state.tracks.iter().map(|t| t.id.clone()).collect();
        state.track_view_layout.set_height(ids[0].clone(), 48.0);
        state.track_view_layout.set_height(ids[2].clone(), 120.0);
        // A VSTi multi-out child channel is mixer-only, like Bus/Return.
        state.tracks[5].id = vsti_output_child_track_id("insert-1", 1);

        let mut session = None;
        let tick = zoom_tick(
            &mut state,
            &mut session,
            1.5,
            0.0,
            ZOOM_VIEWPORT,
            Instant::now(),
        );

        assert!(tick.scroll_y.is_some());
        // 48 × 1.5 is the default height, so the override goes away.
        assert_eq!(state.track_view_layout.height_for(&ids[0]), None);
        assert_eq!(state.track_row_height(&state.tracks[0]), 72.0);
        assert_eq!(state.track_row_height(&state.tracks[1]), 108.0);
        assert_eq!(state.track_row_height(&state.tracks[2]), 180.0);
        for mixer_only in &state.tracks[3..] {
            assert_eq!(
                state.track_view_layout.height_for(&mixer_only.id),
                None,
                "mixer-only channel {} must keep no override",
                mixer_only.id
            );
        }
    }

    #[test]
    fn track_zoom_clamps_rows_to_the_shared_limits() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi, TrackType::Instrument]);
        let id = state.tracks[1].id.clone();
        state.track_view_layout.set_height(id, 120.0);
        let now = Instant::now();

        let mut session = None;
        zoom_tick(&mut state, &mut session, 0.01, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(heights(&state), vec![MIN_TRACK_ROW_HEIGHT; 3]);

        let mut session = None;
        zoom_tick(&mut state, &mut session, 100.0, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(heights(&state), vec![MAX_TRACK_HEIGHT; 3]);
    }

    #[test]
    fn track_zoom_restores_custom_heights_within_a_burst() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi, TrackType::Audio]);
        let ids: Vec<String> = state.tracks.iter().map(|t| t.id.clone()).collect();
        state.track_view_layout.set_height(ids[0].clone(), 48.0);
        // A drag-resized row carries a fractional height.
        state.track_view_layout.set_height(ids[1].clone(), 97.3);
        let before = state.track_view_layout.clone();
        let mut now = Instant::now();
        let mut session = None;

        // Zoom out well past the floor, then back in by exactly as much as
        // actually applied: the burst factor is clamped at the floor.
        for _ in 0..40 {
            now += Duration::from_millis(16);
            zoom_tick(&mut state, &mut session, 0.9, 0.0, ZOOM_VIEWPORT, now);
        }
        assert_eq!(heights(&state), vec![MIN_TRACK_ROW_HEIGHT; 3]);
        let floor = session.as_ref().unwrap().factor;
        now += Duration::from_millis(16);
        zoom_tick(
            &mut state,
            &mut session,
            1.0 / floor,
            0.0,
            ZOOM_VIEWPORT,
            now,
        );

        assert_eq!(state.track_view_layout, before);
    }

    #[test]
    fn track_zoom_reacts_at_once_after_hitting_a_limit() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let tall = state.tracks[1].id.clone();
        state.track_view_layout.set_height(tall.clone(), 240.0);
        let mut now = Instant::now();
        let mut session = None;

        // Far past the ceiling for both rows.
        zoom_tick(&mut state, &mut session, 50.0, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(heights(&state), vec![MAX_TRACK_HEIGHT; 2]);

        // The first tick back already shrinks the row that had the most room.
        now += Duration::from_millis(16);
        let tick = zoom_tick(&mut state, &mut session, 0.9, 0.0, ZOOM_VIEWPORT, now);
        assert!(tick.scroll_y.is_some());
        assert_eq!(state.track_row_height(&state.tracks[0]), 288.0);
        assert_eq!(state.track_row_height(&state.tracks[1]), MAX_TRACK_HEIGHT);

        // Past the floor too: one tick in grows the row that was largest.
        now += Duration::from_millis(16);
        zoom_tick(&mut state, &mut session, 0.001, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(heights(&state), vec![MIN_TRACK_ROW_HEIGHT; 2]);
        now += Duration::from_millis(16);
        let tick = zoom_tick(&mut state, &mut session, 1.1, 0.0, ZOOM_VIEWPORT, now);
        assert!(tick.scroll_y.is_some());
        assert!(state.track_row_height(&state.tracks[1]) > MIN_TRACK_ROW_HEIGHT);
    }

    #[test]
    fn track_zoom_returning_to_the_default_height_removes_the_override() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let small = state.tracks[0].id.clone();
        let default = state.tracks[1].id.clone();
        state.track_view_layout.set_height(small.clone(), 48.0);
        let now = Instant::now();

        let mut session = None;
        zoom_tick(&mut state, &mut session, 1.5, 0.0, ZOOM_VIEWPORT, now);
        // 48 × 1.5 lands exactly on the default: the project saves no height.
        assert_eq!(state.track_view_layout.height_for(&small), None);
        assert_eq!(state.track_view_layout.height_for(&default), Some(108.0));

        let mut session = None;
        zoom_tick(
            &mut state,
            &mut session,
            72.0 / 108.0,
            0.0,
            ZOOM_VIEWPORT,
            now,
        );
        assert_eq!(state.track_view_layout.height_for(&default), None);
        assert_eq!(state.track_view_layout.height_for(&small), Some(48.0));
    }

    #[test]
    fn track_zoom_keeps_the_pointer_on_the_same_point_of_its_track() {
        let mut state = sample_state(&[TrackType::Audio; 10]);
        state.viewport.scroll_y = 100.0;
        let pointer_y = 250.0;
        let (index, offset) = anchor_at(&state, pointer_y);
        let fraction = offset / DEFAULT_TRACK_HEIGHT;

        let scroll_y = zoom_and_scroll(&mut state, 1.5, pointer_y, 400.0).unwrap();

        assert_close(scroll_y, 4.0 * 108.0 + fraction * 108.0 - pointer_y);
        let (after_index, after_offset) = anchor_at(&state, pointer_y);
        assert_eq!(after_index, index);
        assert_close(after_offset / 108.0, fraction);
    }

    #[test]
    fn track_zoom_at_the_top_edge_anchors_the_first_visible_row() {
        let mut state = sample_state(&[TrackType::Audio; 10]);
        state.viewport.scroll_y = 100.0;
        let (index, offset) = anchor_at(&state, 0.0);
        assert_eq!((index, offset), (1, 28.0));

        let scroll_y = zoom_and_scroll(&mut state, 2.0, 0.0, 400.0).unwrap();

        assert_close(scroll_y, 144.0 + 56.0);
        let (after_index, after_offset) = anchor_at(&state, 0.0);
        assert_eq!(after_index, 1);
        assert_close(after_offset, 56.0);
    }

    #[test]
    fn track_zoom_keeps_an_automation_pointer_at_its_pixel_offset() {
        let mut state = sample_state(&[TrackType::Audio; 6]);
        let automated = state.tracks[2].id.clone();
        state.ensure_automation_lane(&automated, AutomationTarget::TrackVolume);
        state.tracks[2].lane_mode = TrackLaneMode::Automation;
        let layout = state.track_row_layout();
        let row = layout.rows[2].clone();
        assert!(row.automation_height > 0.0);
        state.viewport.scroll_y = 50.0;
        // 20 px below the clip row, inside the automation control row.
        let pointer_y = row.y + row.height + 20.0 - state.viewport.scroll_y;

        zoom_and_scroll(&mut state, 1.5, pointer_y, 300.0).unwrap();

        let (index, offset) = anchor_at(&state, pointer_y);
        assert_eq!(index, 2);
        assert_close(offset, 108.0 + 20.0);
        assert_eq!(
            state.track_row_layout().rows[2].automation_height,
            row.automation_height,
            "automation rows do not scale"
        );
    }

    #[test]
    fn track_zoom_anchors_past_collapsed_group_children_and_scales_them() {
        let mut state = sample_state(&[
            TrackType::Group,
            TrackType::Audio,
            TrackType::Audio,
            TrackType::Midi,
            TrackType::Audio,
            TrackType::Audio,
        ]);
        let group_id = state.tracks[0].id.clone();
        let children = [state.tracks[1].id.clone(), state.tracks[2].id.clone()];
        for child in &children {
            assert!(state.assign_track_to_group(child, &group_id));
        }
        assert_eq!(state.toggle_group_collapsed(&group_id), Some(true));
        state.viewport.scroll_y = 30.0;
        let pointer_y = 100.0;
        let (index, offset) = anchor_at(&state, pointer_y);
        let anchored_id = state.tracks[index].id.clone();
        assert!(!children.contains(&anchored_id));

        zoom_and_scroll(&mut state, 1.25, pointer_y, 150.0).unwrap();

        let (after_index, after_offset) = anchor_at(&state, pointer_y);
        assert_eq!(state.tracks[after_index].id, anchored_id);
        assert_close(after_offset / 90.0, offset / 72.0);
        // Hidden children follow the zoom, so they match once expanded.
        for child in &children {
            assert_eq!(state.track_view_layout.height_for(child), Some(90.0));
        }
    }

    #[test]
    fn track_zoom_below_the_last_row_anchors_the_top_row() {
        let mut state = sample_state(&[TrackType::Audio; 3]);
        // All three rows fit, so there is empty lane below the last one.
        let scroll_y = zoom_and_scroll(&mut state, 1.5, 500.0, 600.0);
        assert_eq!(scroll_y, Some(0.0));
        assert_eq!(heights(&state), vec![108.0; 3]);
    }

    #[test]
    fn track_zoom_clamps_scroll_to_the_new_content() {
        let mut state = sample_state(&[TrackType::Audio; 10]);
        let viewport = 500.0;
        state.viewport.scroll_y = 720.0 - viewport;

        // Zoomed out, the content fits the view: nothing left to scroll.
        let scroll_y = zoom_and_scroll(&mut state, 0.5, 390.0, viewport).unwrap();
        assert_eq!(state.total_track_rows_height(), 10.0 * MIN_TRACK_ROW_HEIGHT);
        assert_eq!(scroll_y, 0.0);

        // Zoomed in near the bottom, the scroll stays inside the content.
        let scroll_y = zoom_and_scroll(&mut state, 5.0, 400.0, viewport).unwrap();
        let total = state.total_track_rows_height();
        assert!(total > viewport);
        assert!(scroll_y > 0.0);
        assert!(scroll_y <= total - viewport, "{scroll_y} of {total}");
    }

    /// Every tick that changes a row marks the project, not only a burst's
    /// first: a save that lands mid-burst must not leave the later ticks
    /// looking saved.
    #[test]
    fn track_zoom_marks_the_project_on_every_changing_tick() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let mut now = Instant::now();
        let mut session = None;

        // A tick too small to move any row changes nothing and marks nothing.
        let tick = zoom_tick(&mut state, &mut session, 1.002, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(tick, TrackZoomTick::default());

        now += Duration::from_millis(16);
        let first = zoom_tick(&mut state, &mut session, 1.2, 0.0, ZOOM_VIEWPORT, now);
        assert!(first.scroll_y.is_some() && first.mark_view_dirty);
        now += Duration::from_millis(16);
        let second = zoom_tick(&mut state, &mut session, 1.2, 0.0, ZOOM_VIEWPORT, now);
        assert!(second.scroll_y.is_some() && second.mark_view_dirty);

        // Held at the ceiling, a tick changes nothing and marks nothing.
        now += Duration::from_millis(16);
        zoom_tick(&mut state, &mut session, 50.0, 0.0, ZOOM_VIEWPORT, now);
        now += Duration::from_millis(16);
        let held = zoom_tick(&mut state, &mut session, 1.2, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(held, TrackZoomTick::default());

        // After the burst goes idle the next tick starts a new one.
        now += TRACK_ZOOM_BURST_IDLE + Duration::from_millis(1);
        let next_burst = zoom_tick(&mut state, &mut session, 0.8, 0.0, ZOOM_VIEWPORT, now);
        assert!(next_burst.scroll_y.is_some() && next_burst.mark_view_dirty);
    }

    /// Group `G` (collapsed) holding one child, then two plain tracks. The
    /// visible rows stay at the default height; the hidden child is set to
    /// `child_height`. Returns the state and the child's id.
    fn collapsed_child_state(child_height: f32) -> (TimelineState, String) {
        let mut state = sample_state(&[
            TrackType::Group,
            TrackType::Audio,
            TrackType::Audio,
            TrackType::Midi,
        ]);
        let group_id = state.tracks[0].id.clone();
        let child = state.tracks[1].id.clone();
        assert!(state.assign_track_to_group(&child, &group_id));
        assert_eq!(state.toggle_group_collapsed(&group_id), Some(true));
        state
            .track_view_layout
            .set_height(child.clone(), child_height);
        (state, child)
    }

    fn visible_heights(state: &TimelineState) -> Vec<f32> {
        [0, 2, 3]
            .into_iter()
            .map(|index| state.track_row_height(&state.tracks[index]))
            .collect()
    }

    /// A collapsed group's child with more room to grow than the rows on
    /// screen must not hold the burst past the visible rows' ceiling: the
    /// first tick back shrinks a visible row.
    #[test]
    fn track_zoom_limits_come_from_the_visible_rows_only_at_the_ceiling() {
        let (mut state, child) = collapsed_child_state(TRACK_HEIGHT_SMALL);
        let mut now = Instant::now();
        let mut session = None;

        zoom_tick(&mut state, &mut session, 50.0, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(visible_heights(&state), vec![MAX_TRACK_HEIGHT; 3]);
        // The hidden child still follows the zoom, clamped on its own.
        let child_at_ceiling = state.track_row_height_for_id(&child);
        assert_eq!(child_at_ceiling, (48.0_f32 * 320.0 / 72.0).round());

        now += Duration::from_millis(16);
        let tick = zoom_tick(&mut state, &mut session, 0.9, 0.0, ZOOM_VIEWPORT, now);
        assert!(tick.scroll_y.is_some() && tick.mark_view_dirty);
        assert_eq!(visible_heights(&state), vec![288.0; 3]);
        assert!(state.track_row_height_for_id(&child) < child_at_ceiling);
    }

    /// The same at the floor, with a hidden Huge child that could shrink much
    /// further than the rows on screen.
    #[test]
    fn track_zoom_limits_come_from_the_visible_rows_only_at_the_floor() {
        let (mut state, child) = collapsed_child_state(TRACK_HEIGHT_HUGE);
        let mut now = Instant::now();
        let mut session = None;

        zoom_tick(&mut state, &mut session, 0.001, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(visible_heights(&state), vec![MIN_TRACK_ROW_HEIGHT; 3]);
        let child_at_floor = state.track_row_height_for_id(&child);
        assert_eq!(child_at_floor, (180.0_f32 * 44.0 / 72.0).round());

        now += Duration::from_millis(16);
        let tick = zoom_tick(&mut state, &mut session, 1.1, 0.0, ZOOM_VIEWPORT, now);
        assert!(tick.scroll_y.is_some() && tick.mark_view_dirty);
        assert!(visible_heights(&state)
            .iter()
            .all(|height| *height > MIN_TRACK_ROW_HEIGHT));
    }

    /// A tick that only moves a hidden row still writes it, so the child
    /// matches its siblings once expanded, but changes nothing on screen: no
    /// scroll, no repaint and no unsaved-changes mark.
    #[test]
    fn track_zoom_of_a_hidden_row_alone_does_not_mark_the_project() {
        let (mut state, child) = collapsed_child_state(TRACK_HEIGHT_HUGE);
        let mut session = None;

        // 72 × 1.003 stays within half a pixel; 180 × 1.003 does not.
        let tick = zoom_tick(
            &mut state,
            &mut session,
            1.003,
            0.0,
            ZOOM_VIEWPORT,
            Instant::now(),
        );

        assert_eq!(tick, TrackZoomTick::default());
        assert_eq!(visible_heights(&state), vec![DEFAULT_TRACK_HEIGHT; 3]);
        assert_eq!(state.track_row_height_for_id(&child), 181.0);
    }

    #[test]
    fn track_zoom_restarts_when_another_edit_changed_the_heights() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let edited = state.tracks[0].id.clone();
        let mut now = Instant::now();
        let mut session = None;
        zoom_tick(&mut state, &mut session, 1.5, 0.0, ZOOM_VIEWPORT, now);
        assert_eq!(heights(&state), vec![108.0, 108.0]);

        // A row drag, reset or undo lands between two ticks of one burst.
        state.track_view_layout.set_height(edited, 200.0);
        now += Duration::from_millis(16);
        zoom_tick(&mut state, &mut session, 1.1, 0.0, ZOOM_VIEWPORT, now);

        // Scaled from what is there now, not from the stale burst start.
        assert_eq!(heights(&state), vec![220.0, 119.0]);
    }

    #[test]
    fn track_zoom_restarts_when_the_tracks_change() {
        let mut state = sample_state(&[TrackType::Audio]);
        let mut now = Instant::now();
        let mut session = None;
        zoom_tick(&mut state, &mut session, 1.5, 0.0, ZOOM_VIEWPORT, now);

        let added = sample_state(&[TrackType::Audio, TrackType::Midi]).tracks[1].clone();
        state.tracks.push(added);
        now += Duration::from_millis(16);
        zoom_tick(&mut state, &mut session, 1.5, 0.0, ZOOM_VIEWPORT, now);

        // The new track joins at its own height instead of being skipped.
        assert_eq!(heights(&state), vec![162.0, 108.0]);
    }

    #[test]
    fn track_zoom_waits_for_a_row_resize_drag() {
        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let anchor = state.tracks[0].id.clone();
        state.arm_track_height_resize(&anchor, 100.0, false, false);
        let mut session = None;

        let tick = zoom_tick(
            &mut state,
            &mut session,
            2.0,
            0.0,
            ZOOM_VIEWPORT,
            Instant::now(),
        );

        assert_eq!(tick, TrackZoomTick::default());
        assert!(session.is_none());
        assert_eq!(heights(&state), vec![DEFAULT_TRACK_HEIGHT; 2]);
    }

    /// Track zoom stays out of the undo history, and height edits record
    /// absolute heights. Undoing a row resize made before a zoom therefore
    /// restores that edit's own heights and leaves the zoomed rows alone.
    #[test]
    fn undoing_a_row_resize_after_a_zoom_restores_that_edits_heights() {
        use crate::components::edit::EditCommand;

        let mut state = sample_state(&[TrackType::Audio, TrackType::Midi]);
        let resized = state.tracks[0].id.clone();
        let resize = EditCommand::SetTrackHeights {
            prev: vec![(resized.clone(), DEFAULT_TRACK_HEIGHT)],
            next: vec![(resized.clone(), 120.0)],
        };
        resize.execute(&mut state);
        let mut session = None;
        zoom_tick(
            &mut state,
            &mut session,
            1.5,
            0.0,
            ZOOM_VIEWPORT,
            Instant::now(),
        );
        assert_eq!(heights(&state), vec![180.0, 108.0]);

        resize.undo(&mut state);

        assert_eq!(state.track_view_layout.height_for(&resized), None);
        assert_eq!(heights(&state), vec![DEFAULT_TRACK_HEIGHT, 108.0]);
    }
}
