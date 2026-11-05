//! Audio Connections panel — the editor for the project's logical audio buses.
//!
//! The panel owns **no** routing data. Every row is read from the project's
//! [`AudioConnectionRegistry`] each render, and every edit goes through the
//! registry's structured mutation API. The only state here is transient view
//! state: which tab is showing, which rows are selected, and whether an inline
//! editor or a dropdown is open. Destructive actions are confirmed by the
//! shared message-box window, so no confirmation state lives here.
//!
//! That split is what keeps a stale connection id from surviving a project
//! switch — [`AudioConnectionsPanelState::on_project_changed`] clears the
//! transient state and nothing else needs resetting.

use crate::audio_connections::{
    AudioConnectionDirection, AudioConnectionId, AudioConnectionRegistry, AudioConnectionStatus,
    ChannelLayout,
};

/// Which tab the panel is showing. Session state; never persisted in a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionsTab {
    #[default]
    Inputs,
    Outputs,
}

impl ConnectionsTab {
    pub fn direction(self) -> AudioConnectionDirection {
        match self {
            Self::Inputs => AudioConnectionDirection::Input,
            Self::Outputs => AudioConnectionDirection::Output,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Inputs => "Inputs",
            Self::Outputs => "Outputs",
        }
    }
}

/// An open inline cell editor. Only the name column is inline-editable; the
/// rest are dropdowns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineNameEdit {
    pub connection_id: AudioConnectionId,
    pub draft: String,
}

/// Anchor rectangle of the cell a dropdown was opened from, in window space.
/// Kept as plain floats so this module stays free of GPUI types and the
/// placement logic below is testable without a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellAnchor {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Height of one table row, in logical GPUI units. Every geometry helper here
/// works in logical units — never device pixels — so the same numbers are
/// correct under Windows display scaling, Retina, and fractional scaling.
pub const ROW_HEIGHT: f32 = crate::theme::size::PROMINENT;

/// Resolved width of every table column, in logical units.
///
/// Produced by [`column_widths`] from the width actually available, so the
/// table adapts to the window instead of being sized around one screenshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnWidths {
    pub enabled: f32,
    pub name: f32,
    pub configuration: f32,
    pub device: f32,
    pub left_port: f32,
    pub right_port: f32,
    pub status: f32,
}

impl ColumnWidths {
    /// Fixed, content-sized columns. These never grow — a checkbox and a
    /// short status word gain nothing from extra width.
    pub const ENABLED: f32 = 44.0;
    pub const CONFIGURATION: f32 = 104.0;
    pub const STATUS: f32 = 144.0;

    /// Floors for the flexible columns. Below these the table scrolls
    /// horizontally rather than letting cells collapse into each other.
    pub const MIN_NAME: f32 = 140.0;
    pub const MIN_DEVICE: f32 = 170.0;
    pub const MIN_PORT: f32 = 120.0;

    /// Narrowest table that still shows Device, Left, Right, and Status
    /// legibly. Drives the window's minimum width.
    pub fn min_total() -> f32 {
        Self::ENABLED
            + Self::CONFIGURATION
            + Self::STATUS
            + Self::MIN_NAME
            + Self::MIN_DEVICE
            + 2.0 * Self::MIN_PORT
    }

    pub fn total(&self) -> f32 {
        self.enabled
            + self.name
            + self.configuration
            + self.device
            + self.left_port
            + self.right_port
            + self.status
    }

    /// Left edge of each column, so a popover can be anchored to a cell
    /// without measuring laid-out geometry.
    pub fn offset_of(&self, cell: EditableCell) -> f32 {
        let name = self.enabled;
        let configuration = name + self.name;
        let device = configuration + self.configuration;
        let left = device + self.device;
        match cell {
            EditableCell::Name => name,
            EditableCell::Configuration => configuration,
            EditableCell::Device => device,
            EditableCell::LeftPort => left,
            EditableCell::RightPort => left + self.left_port,
        }
    }

    pub fn width_of(&self, cell: EditableCell) -> f32 {
        match cell {
            EditableCell::Name => self.name,
            EditableCell::Configuration => self.configuration,
            EditableCell::Device => self.device,
            EditableCell::LeftPort => self.left_port,
            EditableCell::RightPort => self.right_port,
        }
    }
}

/// Distribute `available` width across the columns.
///
/// Fixed columns keep their size; the surplus goes to the flexible ones by
/// weight, with Audio Device weighted highest because endpoint names are the
/// longest labels in the table. Below [`ColumnWidths::min_total`] every
/// flexible column sits on its floor and the caller scrolls horizontally.
pub fn column_widths(available: f32) -> ColumnWidths {
    let min_total = ColumnWidths::min_total();
    let surplus = (available - min_total).max(0.0);

    // Device gets the largest share; Left and Right stay equal so the two
    // channel columns remain independently readable.
    const NAME_WEIGHT: f32 = 0.30;
    const DEVICE_WEIGHT: f32 = 0.40;
    const PORT_WEIGHT: f32 = 0.15;

    ColumnWidths {
        enabled: ColumnWidths::ENABLED,
        name: ColumnWidths::MIN_NAME + surplus * NAME_WEIGHT,
        configuration: ColumnWidths::CONFIGURATION,
        device: ColumnWidths::MIN_DEVICE + surplus * DEVICE_WEIGHT,
        left_port: ColumnWidths::MIN_PORT + surplus * PORT_WEIGHT,
        right_port: ColumnWidths::MIN_PORT + surplus * PORT_WEIGHT,
        status: ColumnWidths::STATUS,
    }
}

/// Snap a pointer position to the top of the row it falls in.
///
/// Using the pointer's y (which already accounts for the row list's scroll
/// offset) and quantising it to the row grid gives an exact anchor without
/// measuring laid-out elements. `table_top` is the y of the first row.
pub fn snap_row_top(pointer_y: f32, table_top: f32) -> f32 {
    if pointer_y <= table_top {
        return table_top;
    }
    table_top + ((pointer_y - table_top) / ROW_HEIGHT).floor() * ROW_HEIGHT
}

/// Anchor rectangle for a cell's popover, in the Audio Connections window's
/// own coordinate space.
///
/// Everything here is relative to that window: the x comes from the column
/// layout and the y from the row grid, so the popover stays correct when the
/// utility window is moved, resized, or dragged to another monitor. No value
/// is ever derived from the main project window.
pub fn cell_anchor(
    columns: &ColumnWidths,
    cell: EditableCell,
    pointer_y: f32,
    table_top: f32,
) -> CellAnchor {
    CellAnchor {
        x: columns.offset_of(cell),
        y: snap_row_top(pointer_y, table_top),
        width: columns.width_of(cell),
        height: ROW_HEIGHT,
    }
}

/// Anchor for the cell in row `row_index`, corrected for the table's own
/// scroll offset (`scroll_x`, `scroll_y` are GPUI scroll offsets: `<= 0`,
/// growing negative as the table scrolls right / down).
///
/// The row index is exact where a pointer snap is not: a list scrolled by a
/// fraction of a row put the popover a few pixels off its cell, and a table
/// scrolled sideways put it under the wrong column entirely — which is what
/// made the menu look cropped. Both are gone here because the anchor is the
/// cell's laid-out rectangle, reconstructed from the same numbers the layout
/// used.
pub fn cell_anchor_for_row(
    columns: &ColumnWidths,
    cell: EditableCell,
    row_index: usize,
    table_top: f32,
    scroll_x: f32,
    scroll_y: f32,
) -> CellAnchor {
    CellAnchor {
        x: columns.offset_of(cell) + scroll_x,
        y: table_top + row_index as f32 * ROW_HEIGHT + scroll_y,
        width: columns.width_of(cell),
        height: ROW_HEIGHT,
    }
}

/// Height of one dropdown option row and the menu's inner padding, matching
/// the shared string menu so the estimated height never under-sizes the list
/// (an under-estimate is what cut the last option off and forced a scroll on
/// a four-item menu).
pub const DROPDOWN_ROW_HEIGHT: f32 = 25.0;
pub const DROPDOWN_PADDING: f32 = 8.0;
/// Longest menu before it scrolls.
pub const DROPDOWN_MAX_HEIGHT: f32 = 268.0;

/// Natural height of a menu with `option_count` rows, capped so a long device
/// list scrolls instead of covering the whole window.
pub fn dropdown_height(option_count: usize) -> f32 {
    (option_count as f32 * DROPDOWN_ROW_HEIGHT + DROPDOWN_PADDING).min(DROPDOWN_MAX_HEIGHT)
}

/// Width for a menu whose widest label is `longest_label_chars` characters:
/// at least the cell it opened from, wide enough to show the longest option
/// in full, never wider than the window allows.
///
/// A cell-width menu truncated every real endpoint name ("Focusrite USB ASIO —
/// Analogue 3 + 4" in a 110 px port column), so the list read as cropped.
pub fn dropdown_width(longest_label_chars: usize, anchor_width: f32, viewport_width: f32) -> f32 {
    // 10.5 px UI text averages ~6.3 px per glyph; padding, the check mark and
    // its gap make up the rest — plus the leading icon/avatar column every row
    // now carries, which would otherwise be taken out of the label's width and
    // truncate exactly the endpoint names this function exists to show whole.
    const GLYPH: f32 = 6.3;
    const LEADING_ICON: f32 = 14.0 + 7.0;
    const CHROME: f32 = DROPDOWN_PADDING + 16.0 + 8.0 + 11.0 + 6.0 + LEADING_ICON;
    let wanted = longest_label_chars as f32 * GLYPH + CHROME;
    let max = (viewport_width - 12.0).max(anchor_width.min(viewport_width));
    wanted.max(anchor_width).min(max)
}

/// Which side a popover ended up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopoverSide {
    Below,
    Above,
}

/// Choose the side a dropdown opens on, preferring below and flipping up only
/// when below cannot hold it and above genuinely has more room.
///
/// Mirrors the shared `overlay::resolve_popup_placement` rule so the panel
/// behaves like every other Futureboard popover; kept here as a pure function
/// so orientation can be asserted without native geometry.
pub fn popover_side(anchor: CellAnchor, popup_height: f32, viewport_bottom: f32) -> PopoverSide {
    let space_below = viewport_bottom - (anchor.y + anchor.height);
    let space_above = anchor.y;
    if space_below < popup_height && space_above > space_below {
        PopoverSide::Above
    } else {
        PopoverSide::Below
    }
}

/// Which editable cell has keyboard focus, for Tab traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditableCell {
    Name,
    Configuration,
    Device,
    LeftPort,
    RightPort,
}

impl EditableCell {
    /// Traversal order across a row. `stereo` decides whether the Right port
    /// participates, so Tab never lands on a cell a mono row does not render.
    pub fn order(stereo: bool) -> Vec<EditableCell> {
        let mut cells = vec![
            EditableCell::Name,
            EditableCell::Configuration,
            EditableCell::Device,
            EditableCell::LeftPort,
        ];
        if stereo {
            cells.push(EditableCell::RightPort);
        }
        cells
    }

    pub fn next(self, stereo: bool, delta: i32) -> EditableCell {
        let order = Self::order(stereo);
        let index = order.iter().position(|cell| *cell == self).unwrap_or(0);
        let next = (index as i32 + delta).rem_euclid(order.len() as i32) as usize;
        order[next]
    }
}

/// Which dropdown is open, so only one can be at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenDropdown {
    Layout(AudioConnectionId),
    Device(AudioConnectionId),
    Port {
        connection_id: AudioConnectionId,
        logical_channel: usize,
    },
    AddBus,
    /// The toolbar's preset menu. Anchored to the toolbar rather than to a
    /// table cell, because a preset replaces the whole table.
    Presets,
}

/// Whether removing a bus needs confirmation.
///
/// Only a referenced bus does — removing an unused one is as reversible as
/// adding it back, and a dialog there is noise. The confirmation itself is the
/// shared message-box window, not an overlay drawn inside this panel, so the
/// rest of Studio stays usable while it is up.
pub fn removal_needs_confirmation(affected_tracks: &[String]) -> bool {
    !affected_tracks.is_empty()
}

/// Reset Defaults always confirms: it replaces every bus in the direction,
/// including ones the user created by hand.
pub fn reset_defaults_needs_confirmation() -> bool {
    true
}

/// One rendered table row, derived from the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRow {
    pub id: AudioConnectionId,
    pub name: String,
    pub enabled: bool,
    pub layout: ChannelLayout,
    /// Human-readable endpoint name — never the bus name, and never folded
    /// into it. The Bus Name column stays the user's logical label.
    pub device_label: String,
    /// Left / Mono cell text.
    pub left_port: String,
    /// Right cell text. `None` for a mono bus, so the column renders empty
    /// rather than showing a meaningless dash.
    pub right_port: Option<String>,
    pub status: AudioConnectionStatus,
    pub status_detail: Option<String>,
    pub selected: bool,
}

impl ConnectionRow {
    /// The untruncated label for a cell.
    ///
    /// Narrow columns ellipsize visually, but the complete text stays
    /// available here for the cell tooltip, so nothing a user needs to read is
    /// only reachable by widening the window.
    pub fn full_label(&self, cell: EditableCell) -> String {
        match cell {
            EditableCell::Name => self.name.clone(),
            EditableCell::Configuration => layout_label(self.layout),
            EditableCell::Device => self.device_label.clone(),
            EditableCell::LeftPort => self.left_port.clone(),
            EditableCell::RightPort => self.right_port.clone().unwrap_or_default(),
        }
    }
}

/// Shared text for a channel layout, so the cell, the dropdown, and its
/// tooltip cannot drift apart.
pub fn layout_label(layout: ChannelLayout) -> String {
    match layout {
        ChannelLayout::Mono => "Mono".to_string(),
        ChannelLayout::Stereo => "Stereo".to_string(),
        ChannelLayout::Custom { channels } => format!("{channels} ch"),
    }
}

/// Transient panel state.
#[derive(Debug, Clone, Default)]
pub struct AudioConnectionsPanelState {
    pub tab: ConnectionsTab,
    selected: Vec<AudioConnectionId>,
    pub inline_edit: Option<InlineNameEdit>,
    pub open_dropdown: Option<OpenDropdown>,
    /// Anchor of the cell the open dropdown belongs to.
    pub dropdown_anchor: Option<CellAnchor>,
    /// Focused editable cell for Tab traversal.
    pub focused_cell: Option<EditableCell>,
    /// Warnings from the most recent mutation, shown in the footer.
    pub warnings: Vec<String>,
}

impl AudioConnectionsPanelState {
    pub fn new() -> Self {
        Self::default()
    }

    // ── Tabs ────────────────────────────────────────────────────────────────

    /// Switch tabs. Selection is dropped because ids from the other direction
    /// are meaningless here, and any open editor is closed so a committed edit
    /// cannot land on a row the user can no longer see.
    pub fn set_tab(&mut self, tab: ConnectionsTab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.selected.clear();
        self.close_transient();
    }

    /// Close every open editor and dropdown.
    pub fn close_transient(&mut self) {
        self.inline_edit = None;
        self.open_dropdown = None;
        self.dropdown_anchor = None;
    }

    /// Escape: close the innermost thing first — dropdown, then inline editor.
    /// Returns `true` when something closed, so the caller knows the key was
    /// consumed. Confirmations are separate windows and dismiss themselves.
    pub fn dismiss_topmost(&mut self) -> bool {
        if self.open_dropdown.is_some() {
            self.open_dropdown = None;
            self.dropdown_anchor = None;
            return true;
        }
        if self.inline_edit.is_some() {
            self.inline_edit = None;
            return true;
        }
        false
    }

    /// Move keyboard focus across the editable cells of the selected row.
    pub fn move_cell_focus(&mut self, stereo: bool, delta: i32) {
        let next = match self.focused_cell {
            Some(cell) => cell.next(stereo, delta),
            None if delta >= 0 => EditableCell::Name,
            None => *EditableCell::order(stereo).last().unwrap(),
        };
        self.focused_cell = Some(next);
    }

    /// Reset for a different project. Ids never cross projects.
    pub fn on_project_changed(&mut self) {
        self.selected.clear();
        self.close_transient();
        self.focused_cell = None;
        self.warnings.clear();
    }

    // ── Selection ───────────────────────────────────────────────────────────

    pub fn selected(&self) -> &[AudioConnectionId] {
        &self.selected
    }

    pub fn is_selected(&self, id: &AudioConnectionId) -> bool {
        self.selected.contains(id)
    }

    pub fn select_only(&mut self, id: AudioConnectionId) {
        self.selected = vec![id];
    }

    pub fn toggle_selected(&mut self, id: AudioConnectionId) {
        match self.selected.iter().position(|existing| *existing == id) {
            Some(index) => {
                self.selected.remove(index);
            }
            None => self.selected.push(id),
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// The single selected connection, when exactly one is selected. Toolbar
    /// actions that only make sense for one row use this.
    pub fn single_selection(&self) -> Option<&AudioConnectionId> {
        match self.selected.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// Drop selections that no longer exist — after a removal or a project
    /// reload.
    pub fn prune_selection(&mut self, registry: &AudioConnectionRegistry) {
        self.selected.retain(|id| registry.get(id).is_some());
        if let Some(edit) = &self.inline_edit {
            if registry.get(&edit.connection_id).is_none() {
                self.inline_edit = None;
            }
        }
    }

    /// Move the selection up or down within the current tab's rows.
    pub fn move_selection(&mut self, registry: &AudioConnectionRegistry, delta: i32) {
        let rows: Vec<AudioConnectionId> = registry
            .by_direction(self.tab.direction())
            .into_iter()
            .map(|connection| connection.id.clone())
            .collect();
        if rows.is_empty() {
            return;
        }
        let current = self
            .single_selection()
            .and_then(|id| rows.iter().position(|row| row == id));
        let next = match current {
            Some(index) => (index as i32 + delta).clamp(0, rows.len() as i32 - 1) as usize,
            // No selection: Down lands on the first row, Up on the last.
            None if delta >= 0 => 0,
            None => rows.len() - 1,
        };
        self.select_only(rows[next].clone());
    }

    // ── Inline rename ───────────────────────────────────────────────────────

    pub fn begin_rename(&mut self, registry: &AudioConnectionRegistry, id: &AudioConnectionId) {
        let Some(connection) = registry.get(id) else {
            return;
        };
        self.open_dropdown = None;
        self.inline_edit = Some(InlineNameEdit {
            connection_id: id.clone(),
            draft: connection.name.clone(),
        });
    }

    pub fn update_rename_draft(&mut self, draft: impl Into<String>) {
        if let Some(edit) = self.inline_edit.as_mut() {
            edit.draft = draft.into();
        }
    }

    pub fn cancel_rename(&mut self) {
        self.inline_edit = None;
    }

    /// Commit the open rename. Returns the `(id, draft)` for the caller to
    /// push through `registry.update_name`, or `None` when nothing is open.
    pub fn take_rename(&mut self) -> Option<(AudioConnectionId, String)> {
        self.inline_edit
            .take()
            .map(|edit| (edit.connection_id, edit.draft))
    }

    // ── Dropdowns ───────────────────────────────────────────────────────────

    /// Open (or close) a dropdown anchored to a cell.
    pub fn toggle_dropdown_at(&mut self, dropdown: OpenDropdown, anchor: CellAnchor) {
        let already_open = self.open_dropdown.as_ref() == Some(&dropdown);
        self.toggle_dropdown(dropdown);
        self.dropdown_anchor = (!already_open).then_some(anchor);
    }

    pub fn toggle_dropdown(&mut self, dropdown: OpenDropdown) {
        // Opening a dropdown ends any inline edit, so a stale draft can never
        // be committed by a later click elsewhere.
        self.inline_edit = None;
        self.open_dropdown = if self.open_dropdown.as_ref() == Some(&dropdown) {
            None
        } else {
            Some(dropdown)
        };
    }

    pub fn close_dropdown(&mut self) {
        self.open_dropdown = None;
        self.dropdown_anchor = None;
    }

    pub fn is_dropdown_open(&self, dropdown: &OpenDropdown) -> bool {
        self.open_dropdown.as_ref() == Some(dropdown)
    }

    // ── Warnings ────────────────────────────────────────────────────────────

    pub fn set_warnings(&mut self, warnings: Vec<String>) {
        self.warnings = warnings;
    }

    // ── Rows ────────────────────────────────────────────────────────────────

    /// Build the rows for the active tab.
    ///
    /// `ports` supplies the human-readable device name; without it the table
    /// would fall back to showing an opaque device id.
    pub fn rows(
        &self,
        registry: &AudioConnectionRegistry,
        ports: &crate::audio_connections::AvailablePorts,
    ) -> Vec<ConnectionRow> {
        registry
            .by_direction(self.tab.direction())
            .into_iter()
            .map(|connection| {
                let is_stereo = connection.channel_layout.channel_count() > 1;
                let port_text = |logical: usize| {
                    connection
                        .binding(logical)
                        .map(|binding| binding.physical_port_id.port_name.clone())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "—".to_string())
                };
                ConnectionRow {
                    id: connection.id.clone(),
                    name: connection.name.clone(),
                    enabled: connection.enabled,
                    layout: connection.channel_layout,
                    device_label: registry
                        .device_display_name(&connection.id, ports)
                        .unwrap_or_else(|| "Unassigned".to_string()),
                    left_port: port_text(0),
                    right_port: is_stereo.then(|| port_text(1)),
                    status: connection.status,
                    status_detail: status_detail(connection.status),
                    selected: self.is_selected(&connection.id),
                }
            })
            .collect()
    }

    /// Footer summary: `(row count, count of rows needing attention)`.
    pub fn summary(&self, registry: &AudioConnectionRegistry) -> (usize, usize) {
        let rows = registry.by_direction(self.tab.direction());
        let attention = rows
            .iter()
            .filter(|connection| {
                !matches!(
                    connection.status,
                    AudioConnectionStatus::Active | AudioConnectionStatus::Disabled
                )
            })
            .count();
        (rows.len(), attention)
    }
}

/// Longer explanation for a status, shown as the cell tooltip. `None` when the
/// short label already says everything.
pub fn status_detail(status: AudioConnectionStatus) -> Option<String> {
    match status {
        AudioConnectionStatus::Active => None,
        AudioConnectionStatus::DeviceMissing => {
            Some("The assigned audio device is not currently available.".to_string())
        }
        AudioConnectionStatus::StandIn => Some(
            "The assigned audio device is not available, so this bus uses the same ports on \
             the device selected in Audio Device Setup. The assignment is kept and takes over \
             again when that device is back."
                .to_string(),
        ),
        AudioConnectionStatus::PortMissing => {
            Some("The device is available but an assigned port is not.".to_string())
        }
        AudioConnectionStatus::Disconnected => {
            Some("No physical ports are assigned yet.".to_string())
        }
        AudioConnectionStatus::Disabled => {
            Some("Disabled. Mappings are kept and this bus resolves to silence.".to_string())
        }
        AudioConnectionStatus::Conflict => Some(
            "This mapping collides with another assignment. Left and Right must differ, and two \
             outputs must not write the same port."
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_connections::{AudioConnection, AvailablePorts};

    // ── Dropdown geometry ────────────────────────────────────────────────────

    /// The anchor is the cell's laid-out rectangle: row index times row height
    /// from the table top, shifted by however far the table has scrolled.
    #[test]
    fn row_anchor_follows_the_tables_own_scroll_offset() {
        let columns = column_widths(1000.0);
        let unscrolled = cell_anchor_for_row(&columns, EditableCell::Device, 3, 100.0, 0.0, 0.0);
        assert_eq!(unscrolled.x, columns.offset_of(EditableCell::Device));
        assert_eq!(unscrolled.y, 100.0 + 3.0 * ROW_HEIGHT);
        assert_eq!(unscrolled.width, columns.device);
        assert_eq!(unscrolled.height, ROW_HEIGHT);

        // Scrolled 40 px right and one and a half rows down: the popover must
        // move with the cell, not stay where the unscrolled cell was.
        let scrolled = cell_anchor_for_row(
            &columns,
            EditableCell::Device,
            3,
            100.0,
            -40.0,
            -1.5 * ROW_HEIGHT,
        );
        assert_eq!(scrolled.x, unscrolled.x - 40.0);
        assert_eq!(scrolled.y, unscrolled.y - 1.5 * ROW_HEIGHT);
    }

    /// A short menu is exactly as tall as its rows, so nothing is cut off and
    /// nothing scrolls; a long one caps and scrolls.
    #[test]
    fn dropdown_height_fits_every_row_until_the_cap() {
        assert_eq!(
            dropdown_height(4),
            4.0 * DROPDOWN_ROW_HEIGHT + DROPDOWN_PADDING
        );
        assert_eq!(dropdown_height(40), DROPDOWN_MAX_HEIGHT);
    }

    /// The menu grows past its cell to show the longest label, but never past
    /// the window.
    #[test]
    fn dropdown_width_fits_the_longest_label_within_the_window() {
        // Short labels: the cell width wins.
        assert_eq!(dropdown_width(4, 150.0, 1000.0), 150.0);
        // A long endpoint name widens the menu past a narrow port column.
        let wide = dropdown_width(34, 110.0, 1000.0);
        assert!(wide > 200.0, "{wide}");
        // But a tiny window clamps it.
        assert_eq!(dropdown_width(80, 110.0, 300.0), 288.0);
    }

    fn ports() -> AvailablePorts {
        AvailablePorts::for_device("dev-1", "Interface", 4, 4)
    }

    fn registry() -> AudioConnectionRegistry {
        AudioConnectionRegistry::default_template(&ports(), "dev-1")
    }

    // ── Tabs ────────────────────────────────────────────────────────────────

    #[test]
    fn the_inputs_tab_shows_only_input_connections() {
        let registry = registry();
        let panel = AudioConnectionsPanelState::new();
        assert_eq!(panel.tab, ConnectionsTab::Inputs);
        let rows = panel.rows(&registry, &ports());
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.name.contains("Input")));
    }

    #[test]
    fn the_outputs_tab_shows_only_output_connections() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        panel.set_tab(ConnectionsTab::Outputs);
        let rows = panel.rows(&registry, &ports());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Main Output 1-2");
    }

    #[test]
    fn switching_tabs_clears_selection_and_open_editors() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        panel.select_only(id.clone());
        panel.begin_rename(&registry, &id);
        panel.toggle_dropdown(OpenDropdown::Device(id));

        panel.set_tab(ConnectionsTab::Outputs);
        assert!(panel.selected().is_empty());
        assert!(panel.inline_edit.is_none());
        assert!(panel.open_dropdown.is_none());
    }

    // ── Rows ────────────────────────────────────────────────────────────────

    #[test]
    fn a_mono_row_has_no_right_port_cell_and_a_stereo_row_does() {
        let registry = registry();
        let panel = AudioConnectionsPanelState::new();
        let rows = panel.rows(&registry, &ports());

        let mono = rows
            .iter()
            .find(|row| row.layout == ChannelLayout::Mono)
            .unwrap();
        assert!(mono.right_port.is_none());
        assert_eq!(mono.left_port, "Input 1");

        let stereo = rows
            .iter()
            .find(|row| row.layout == ChannelLayout::Stereo)
            .unwrap();
        assert_eq!(stereo.left_port, "Input 1");
        assert_eq!(stereo.right_port.as_deref(), Some("Input 2"));
    }

    #[test]
    fn the_footer_counts_rows_needing_attention() {
        let mut registry = registry();
        let panel = AudioConnectionsPanelState::new();
        assert_eq!(panel.summary(&registry), (3, 0));

        registry.revalidate(&AvailablePorts::default());
        let (rows, attention) = panel.summary(&registry);
        assert_eq!(rows, 3);
        assert_eq!(attention, 3, "every row is DeviceMissing");
    }

    // ── Selection / keyboard ────────────────────────────────────────────────

    #[test]
    fn arrow_selection_walks_the_active_tab_and_clamps_at_the_ends() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let rows: Vec<_> = registry
            .by_direction(AudioConnectionDirection::Input)
            .into_iter()
            .map(|c| c.id.clone())
            .collect();

        panel.move_selection(&registry, 1);
        assert_eq!(panel.single_selection(), Some(&rows[0]));
        panel.move_selection(&registry, 1);
        assert_eq!(panel.single_selection(), Some(&rows[1]));
        panel.move_selection(&registry, -1);
        assert_eq!(panel.single_selection(), Some(&rows[0]));
        // Already at the top — stays put rather than wrapping.
        panel.move_selection(&registry, -1);
        assert_eq!(panel.single_selection(), Some(&rows[0]));
    }

    #[test]
    fn removed_rows_are_pruned_from_the_selection_and_editor() {
        let mut registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        panel.select_only(id.clone());
        panel.begin_rename(&registry, &id);

        registry.remove(&id);
        panel.prune_selection(&registry);
        assert!(panel.selected().is_empty());
        assert!(panel.inline_edit.is_none());
    }

    #[test]
    fn single_selection_is_none_when_several_rows_are_selected() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let rows = registry.by_direction(AudioConnectionDirection::Input);
        panel.select_only(rows[0].id.clone());
        panel.toggle_selected(rows[1].id.clone());
        assert_eq!(panel.selected().len(), 2);
        assert!(panel.single_selection().is_none());

        panel.toggle_selected(rows[1].id.clone());
        assert_eq!(panel.single_selection(), Some(&rows[0].id));
    }

    // ── Inline rename ───────────────────────────────────────────────────────

    #[test]
    fn a_committed_rename_hands_back_the_draft_and_keeps_the_id() {
        let mut registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();

        panel.begin_rename(&registry, &id);
        panel.update_rename_draft("  Microphone  ");
        let (edited_id, draft) = panel.take_rename().expect("rename open");
        assert_eq!(edited_id, id);

        let mutation = registry.update_name(&edited_id, &draft);
        assert_eq!(registry.name_of(&id), Some("Microphone"), "trimmed");
        assert!(
            !mutation.needs_routing_rebuild,
            "a name change must not rebuild routing"
        );
        assert!(registry.get(&id).is_some(), "the id is unchanged");
    }

    #[test]
    fn cancelling_a_rename_discards_the_draft() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        panel.begin_rename(&registry, &id);
        panel.update_rename_draft("Discarded");
        panel.cancel_rename();
        assert!(panel.take_rename().is_none());
    }

    #[test]
    fn opening_a_dropdown_ends_an_inline_edit() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        panel.begin_rename(&registry, &id);
        panel.toggle_dropdown(OpenDropdown::Layout(id));
        assert!(
            panel.inline_edit.is_none(),
            "a stale draft must not survive to be committed by a later click"
        );
    }

    #[test]
    fn only_one_dropdown_is_open_at_a_time() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        panel.toggle_dropdown(OpenDropdown::Layout(id.clone()));
        assert!(panel.is_dropdown_open(&OpenDropdown::Layout(id.clone())));

        panel.toggle_dropdown(OpenDropdown::Device(id.clone()));
        assert!(!panel.is_dropdown_open(&OpenDropdown::Layout(id.clone())));
        assert!(panel.is_dropdown_open(&OpenDropdown::Device(id.clone())));

        // Toggling the same one closes it.
        panel.toggle_dropdown(OpenDropdown::Device(id));
        assert!(panel.open_dropdown.is_none());
    }

    // ── Project switching ───────────────────────────────────────────────────

    #[test]
    fn switching_projects_drops_every_reference_to_the_old_one() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        panel.select_only(id.clone());
        panel.begin_rename(&registry, &id);
        panel.set_warnings(vec!["stale".to_string()]);

        panel.on_project_changed();
        assert!(panel.selected().is_empty());
        assert!(panel.inline_edit.is_none());
        assert!(panel.warnings.is_empty());
    }

    // ── Confirmation ────────────────────────────────────────────────────────

    #[test]
    fn removing_an_unreferenced_connection_needs_no_confirmation() {
        assert!(!removal_needs_confirmation(&[]));
    }

    #[test]
    fn removing_a_referenced_connection_is_destructive() {
        assert!(removal_needs_confirmation(&[
            "track-1".to_string(),
            "track-2".to_string()
        ]));
    }

    #[test]
    fn reset_defaults_always_confirms() {
        assert!(reset_defaults_needs_confirmation());
    }

    // ── Status presentation ─────────────────────────────────────────────────

    #[test]
    fn every_non_active_status_has_an_explanation() {
        for status in [
            AudioConnectionStatus::DeviceMissing,
            AudioConnectionStatus::PortMissing,
            AudioConnectionStatus::Disconnected,
            AudioConnectionStatus::Disabled,
            AudioConnectionStatus::Conflict,
        ] {
            assert!(
                status_detail(status).is_some(),
                "{status:?} needs a tooltip"
            );
            assert!(!status.label().is_empty());
        }
        assert!(status_detail(AudioConnectionStatus::Active).is_none());
    }

    /// A connection loaded from a project with no device and no bindings must
    /// render without panicking.
    #[test]
    fn a_malformed_connection_renders_safely() {
        let mut registry = AudioConnectionRegistry::new();
        registry.add(AudioConnection::new(
            "Broken",
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
        ));
        registry.revalidate(&AvailablePorts::default());

        let panel = AudioConnectionsPanelState::new();
        let rows = panel.rows(&registry, &ports());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_port, "—");
        assert_eq!(rows[0].right_port.as_deref(), Some("—"));
        assert_eq!(rows[0].device_label, "Unassigned");
        assert_eq!(rows[0].status, AudioConnectionStatus::Disconnected);
    }

    // ── Popover orientation ─────────────────────────────────────────────────

    /// A row near the top opens downward; one near the bottom flips upward
    /// rather than clipping.
    #[test]
    fn a_dropdown_near_the_bottom_edge_flips_upward() {
        let near_top = CellAnchor {
            x: 10.0,
            y: 40.0,
            width: 120.0,
            height: 24.0,
        };
        assert_eq!(popover_side(near_top, 120.0, 460.0), PopoverSide::Below);

        let near_bottom = CellAnchor {
            x: 10.0,
            y: 400.0,
            width: 120.0,
            height: 24.0,
        };
        assert_eq!(popover_side(near_bottom, 120.0, 460.0), PopoverSide::Above);
    }

    /// With too little room on either side, it stays on the preferred side
    /// rather than oscillating.
    #[test]
    fn a_dropdown_with_no_room_either_side_stays_below() {
        let cramped = CellAnchor {
            x: 0.0,
            y: 10.0,
            width: 120.0,
            height: 24.0,
        };
        assert_eq!(popover_side(cramped, 500.0, 60.0), PopoverSide::Below);
    }

    // ── Cell focus traversal ────────────────────────────────────────────────

    #[test]
    fn tab_traversal_skips_the_right_port_on_a_mono_row() {
        assert_eq!(
            EditableCell::order(false),
            vec![
                EditableCell::Name,
                EditableCell::Configuration,
                EditableCell::Device,
                EditableCell::LeftPort
            ]
        );
        assert_eq!(
            EditableCell::order(true).last(),
            Some(&EditableCell::RightPort)
        );
        // Wraps rather than running off the end.
        assert_eq!(
            EditableCell::LeftPort.next(false, 1),
            EditableCell::Name,
            "a mono row wraps from Left/Mono back to Name"
        );
        assert_eq!(
            EditableCell::LeftPort.next(true, 1),
            EditableCell::RightPort
        );
    }

    #[test]
    fn shift_tab_walks_backwards_and_wraps() {
        let mut panel = AudioConnectionsPanelState::new();
        panel.move_cell_focus(true, 1);
        assert_eq!(panel.focused_cell, Some(EditableCell::Name));
        panel.move_cell_focus(true, -1);
        assert_eq!(
            panel.focused_cell,
            Some(EditableCell::RightPort),
            "backwards from Name wraps to the last cell"
        );
    }

    // ── Escape ordering ─────────────────────────────────────────────────────

    /// Escape closes the innermost surface first so one press never discards
    /// two things at once.
    #[test]
    fn escape_closes_the_dropdown_before_the_editor() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();

        panel.begin_rename(&registry, &id);
        panel.toggle_dropdown_at(
            OpenDropdown::Device(id.clone()),
            CellAnchor {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 24.0,
            },
        );

        // Only the dropdown closes on the first press.
        assert!(panel.dismiss_topmost());
        assert!(panel.open_dropdown.is_none());

        // Opening the dropdown had already ended the inline edit, so there is
        // nothing left for a second press to close.
        assert!(!panel.dismiss_topmost(), "nothing left to close");

        // With an editor open and no dropdown, Escape closes the editor.
        panel.begin_rename(&registry, &id);
        assert!(panel.dismiss_topmost());
        assert!(panel.inline_edit.is_none());
    }

    #[test]
    fn opening_a_dropdown_records_its_anchor_and_closing_clears_it() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        let anchor = CellAnchor {
            x: 12.0,
            y: 80.0,
            width: 140.0,
            height: 24.0,
        };

        panel.toggle_dropdown_at(OpenDropdown::Layout(id.clone()), anchor);
        assert_eq!(panel.dropdown_anchor, Some(anchor));

        panel.close_dropdown();
        assert!(panel.dropdown_anchor.is_none());

        // Toggling the same dropdown closed also clears the anchor.
        panel.toggle_dropdown_at(OpenDropdown::Layout(id.clone()), anchor);
        panel.toggle_dropdown_at(OpenDropdown::Layout(id), anchor);
        assert!(panel.open_dropdown.is_none());
        assert!(panel.dropdown_anchor.is_none());
    }

    #[test]
    fn switching_tabs_and_project_change_clear_the_dropdown_anchor() {
        let registry = registry();
        let mut panel = AudioConnectionsPanelState::new();
        let id = registry.by_direction(AudioConnectionDirection::Input)[0]
            .id
            .clone();
        let anchor = CellAnchor {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
        };

        panel.toggle_dropdown_at(OpenDropdown::Device(id.clone()), anchor);
        panel.set_tab(ConnectionsTab::Outputs);
        assert!(panel.dropdown_anchor.is_none());

        panel.toggle_dropdown_at(OpenDropdown::Device(id), anchor);
        panel.focused_cell = Some(EditableCell::Device);
        panel.on_project_changed();
        assert!(panel.dropdown_anchor.is_none());
        assert!(panel.focused_cell.is_none());
    }
}
