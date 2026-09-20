//! Workspace layout save / restore — wires `workspace_layout.rs` into `StudioLayout`.
//!
//! Two public methods are exposed on `StudioLayout`:
//!
//! - `capture_workspace_layout()` — snapshot current state into a
//!   `SavedWorkspaceLayout` (no I/O).
//! - `save_workspace_layout()` — capture + write to disk.
//! - `restore_workspace_layout(layout)` — apply a loaded layout to self.
//!
//! The callers are:
//! - `new()` — calls `restore_workspace_layout` with the loaded layout.
//! - Shutdown path — calls `save_workspace_layout`.

use crate::layout::studio_state::RightDockTab;
use crate::workspace_layout::{
    load_or_default_workspace_layout, save_workspace_layout as persist_to_disk,
    SavedBottomTab, SavedMixerView, SavedPanelVisibility, SavedRightDockTab,
    SavedWorkspaceLayout,
};

use super::StudioLayout;

// ── Tab conversions ───────────────────────────────────────────────────────────

fn bottom_tab_to_saved(
    tab: crate::components::BottomTab,
) -> SavedBottomTab {
    use crate::components::BottomTab;
    match tab {
        BottomTab::Mixer => SavedBottomTab::Mixer,
        BottomTab::Editor => SavedBottomTab::Editor,
        BottomTab::EffectEditor => SavedBottomTab::EffectEditor,
    }
}

fn bottom_tab_from_saved(
    tab: SavedBottomTab,
) -> crate::components::BottomTab {
    use crate::components::BottomTab;
    match tab {
        SavedBottomTab::Mixer => BottomTab::Mixer,
        SavedBottomTab::Editor => BottomTab::Editor,
        SavedBottomTab::EffectEditor => BottomTab::EffectEditor,
    }
}

fn right_dock_tab_to_saved(tab: RightDockTab) -> SavedRightDockTab {
    match tab {
        RightDockTab::Inspector => SavedRightDockTab::Inspector,
        RightDockTab::ChordDisplay => SavedRightDockTab::ChordDisplay,
        RightDockTab::LyricDisplay => SavedRightDockTab::LyricDisplay,
        RightDockTab::LyricEditor => SavedRightDockTab::LyricEditor,
        RightDockTab::Solfege => SavedRightDockTab::Solfege,
    }
}

fn right_dock_tab_from_saved(tab: SavedRightDockTab) -> RightDockTab {
    match tab {
        SavedRightDockTab::Inspector => RightDockTab::Inspector,
        SavedRightDockTab::ChordDisplay => RightDockTab::ChordDisplay,
        SavedRightDockTab::LyricDisplay => RightDockTab::LyricDisplay,
        SavedRightDockTab::LyricEditor => RightDockTab::LyricEditor,
        SavedRightDockTab::Solfege => RightDockTab::Solfege,
    }
}

// ── StudioLayout impl ─────────────────────────────────────────────────────────

impl StudioLayout {
    /// Snapshot the current layout state into a `SavedWorkspaceLayout`.
    ///
    /// Pure — no `cx.notify()`. Call this from the shutdown path.
    pub fn capture_workspace_layout(&self, cx: &mut gpui::App) -> SavedWorkspaceLayout {
        let panels = SavedPanelVisibility {
            browser: self.panels.browser,
            inspector: self.panels.inspector,
            bottom_docked: self.panels.bottom_docked,
        };

        let bottom_panel_height_px = self.bottom_panel_state.height_px;
        let active_bottom_tab = bottom_tab_to_saved(self.active_bottom_tab);
        let right_dock_tab = right_dock_tab_to_saved(self.right_dock_tab);

        let mixer = SavedMixerView {
            scroll_x: self.mixer_view.scroll_x,
            insert_section_px: self.mixer_view.insert_section_px,
            send_section_px: self.mixer_view.send_section_px,
            tree_sidebar_width_px: self.mixer_view.tree_sidebar_width_px,
        };

        // Collect secondary window bounds if they are currently open.
        let secondary_windows = self.capture_secondary_window_bounds(cx);

        SavedWorkspaceLayout {
            version: 1,
            panels,
            bottom_panel_height_px,
            active_bottom_tab,
            right_dock_tab,
            mixer,
            secondary_windows,
        }
    }

    /// Collect open secondary window bounds.
    fn capture_secondary_window_bounds(
        &self,
        cx: &mut gpui::App,
    ) -> crate::workspace_layout::SavedSecondaryWindows {
        use crate::workspace_layout::{SavedSecondaryWindows, SavedWindowBounds};

        let mixer_bounds = self
            .external_windows
            .mixer
            .as_ref()
            .and_then(|handle| {
                handle
                    .update(cx, |_, window, _| SavedWindowBounds::from_gpui(window.bounds()))
                    .ok()
            });

        let big_clock_bounds = self
            .external_windows
            .big_clock
            .as_ref()
            .and_then(|handle| {
                handle
                    .update(cx, |_, window, _| SavedWindowBounds::from_gpui(window.bounds()))
                    .ok()
            });

        let timecode_bounds = self
            .external_windows
            .timecode
            .as_ref()
            .and_then(|handle| {
                handle
                    .update(cx, |_, window, _| SavedWindowBounds::from_gpui(window.bounds()))
                    .ok()
            });

        SavedSecondaryWindows {
            mixer: mixer_bounds,
            big_clock: big_clock_bounds,
            timecode: timecode_bounds,
        }
    }

    /// Capture and write the workspace layout to disk.
    ///
    /// Called at shutdown (project close + app quit). Non-fatal on I/O error.
    pub fn save_workspace_layout(&self, cx: &mut gpui::App) {
        let layout = self.capture_workspace_layout(cx);
        persist_to_disk(&layout);
    }

    /// Apply a previously loaded `SavedWorkspaceLayout` to this `StudioLayout`.
    ///
    /// Called once during `new()` after the layout file has been read.
    /// Does not call `cx.notify()` — the caller's initial render covers that.
    pub fn restore_workspace_layout(&mut self, layout: &SavedWorkspaceLayout) {
        // Panel visibility.
        self.panels.browser = layout.panels.browser;
        self.panels.inspector = layout.panels.inspector;
        self.panels.bottom_docked = layout.panels.bottom_docked;

        // Bottom panel height (clamp inside BottomPanelState limits).
        let clamped_h = layout
            .bottom_panel_height_px
            .clamp(self.bottom_panel_state.min_height_px, self.bottom_panel_state.max_height_px);
        self.bottom_panel_state.height_px = clamped_h;

        // Active tabs.
        self.active_bottom_tab = bottom_tab_from_saved(layout.active_bottom_tab);
        self.right_dock_tab = right_dock_tab_from_saved(layout.right_dock_tab);

        // Mixer view state.
        self.mixer_view.scroll_x = layout.mixer.scroll_x;
        self.mixer_view.insert_section_px = layout.mixer.insert_section_px;
        self.mixer_view.send_section_px = layout.mixer.send_section_px;
        self.mixer_view.tree_sidebar_width_px = layout.mixer.tree_sidebar_width_px;
    }

    /// Load the layout file from disk and apply it to `self`.
    ///
    /// Call this once from `StudioLayout::new()` after field initialisation.
    pub fn load_and_restore_workspace_layout(&mut self) {
        let layout = load_or_default_workspace_layout();
        self.restore_workspace_layout(&layout);
        // Stash the secondary window state so the deferred open can read it.
        self.pending_secondary_window_restore = layout.secondary_windows;
    }

    /// Open any secondary windows that were visible on last shutdown.
    ///
    /// Called via `cx.defer` from `new()` so the entity is fully initialised.
    pub(super) fn restore_secondary_windows_from_layout(&mut self, cx: &mut gpui::Context<Self>) {
        use crate::components::clock_window::ClockKind;

        // Clone the pending state out — we must not hold a borrow while calling open methods.
        let pending = std::mem::take(&mut self.pending_secondary_window_restore);

        // Mixer window.
        if let Some(ref saved) = pending.mixer {
            let owner = Some(saved.to_gpui());
            self.open_mixer_external_window(owner, cx);
        }

        // Big clock window.
        if let Some(ref saved) = pending.big_clock {
            let owner = Some(saved.to_gpui());
            self.open_clock_window(ClockKind::BigClock, owner, cx);
        }

        // Timecode window.
        if let Some(ref saved) = pending.timecode {
            let owner = Some(saved.to_gpui());
            self.open_clock_window(ClockKind::Timecode, owner, cx);
        }
    }
}
