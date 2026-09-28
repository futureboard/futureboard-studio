use std::time::{Duration, Instant};

use gpui::{Context, DragMoveEvent, MouseDownEvent, Window};

use crate::components::{
    status_content_signature, BottomPanelResizeDrag, BottomPanelState, BottomTab, StatusBarContent,
};
use crate::layout::{RightDockTab, WorkspaceActivePanel};

use super::StudioLayout;

/// What a bottom-dock tab toggle (the Mixer chrome button, a menu item) does
/// on its next press.
///
/// The action is derived from the same three values the button's lit/unlit
/// state is derived from, which is the whole point of naming it: the chrome
/// reported "Mixer visible" as *docked AND the Mixer tab is active*, while the
/// press only flipped `docked`. Standing on the Editor tab the button was
/// therefore drawn unlit, and pressing it — to show the mixer — hid the dock
/// instead; pressing it again brought the dock back still on the Editor, with
/// the button still unlit. Indicator and action now read the same state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomTabToggleAction {
    /// The tab is already the one on screen — hide the whole dock.
    CloseDock,
    /// The dock is hidden, or showing a different tab: show it on this tab.
    ShowTab,
}

pub(crate) fn bottom_tab_toggle_action(
    docked: bool,
    active_tab: BottomTab,
    tab: BottomTab,
) -> BottomTabToggleAction {
    if docked && active_tab == tab {
        BottomTabToggleAction::CloseDock
    } else {
        BottomTabToggleAction::ShowTab
    }
}

impl StudioLayout {
    pub(crate) fn bottom_panel_docked(&self) -> bool {
        self.panels.bottom_docked
    }

    /// Show the bottom dock on `tab`, docking it first if it was hidden.
    ///
    /// The single "open the dock at X" path. Opening used to be spelled inline
    /// at each call site as `bottom_docked = true` plus some subset of the
    /// follow-up work, which is how the dock ended up open on a tab nobody had
    /// asked for.
    pub(crate) fn show_bottom_panel_tab(&mut self, tab: BottomTab, cx: &mut Context<Self>) {
        self.panels.bottom_docked = true;
        self.ensure_mixer_tree_defaults_once(cx);
        self.ensure_mixer_tree_ui_hooks(cx.entity().clone(), cx);
        self.set_active_bottom_tab(tab, cx);
        self.sync_timeline_chrome_metrics(cx);
        self.notify_bottom_panel_shell(cx);
        cx.notify();
    }

    /// Show or hide the bottom dock without choosing a tab for the user.
    ///
    /// This is what the chrome's panel button does. `toggle_bottom_panel_tab`
    /// is the *other* gesture — "show me the Mixer" — and using it for the
    /// panel button is what left the Editor out: standing on the Editor tab the
    /// button read unlit and switched the dock to the Mixer instead of closing
    /// it, so there was no single control that hid the dock you were looking
    /// at. Reopening restores `active_bottom_tab`, which `close_bottom_panel`
    /// deliberately leaves alone.
    pub(crate) fn toggle_bottom_panel(&mut self, cx: &mut Context<Self>) {
        if self.panels.bottom_docked {
            self.close_bottom_panel(cx);
        } else {
            self.show_bottom_panel_tab(self.active_bottom_tab, cx);
        }
    }

    /// Press the dock toggle that belongs to `tab`.
    pub(crate) fn toggle_bottom_panel_tab(&mut self, tab: BottomTab, cx: &mut Context<Self>) {
        match bottom_tab_toggle_action(self.panels.bottom_docked, self.active_bottom_tab, tab) {
            BottomTabToggleAction::CloseDock => self.close_bottom_panel(cx),
            BottomTabToggleAction::ShowTab => self.show_bottom_panel_tab(tab, cx),
        }
    }

    pub(crate) fn active_bottom_tab(&self) -> BottomTab {
        self.active_bottom_tab
    }

    pub(crate) fn active_panel(&self) -> WorkspaceActivePanel {
        self.active_panel
    }

    pub(crate) fn set_active_panel(&mut self, panel: WorkspaceActivePanel, cx: &mut Context<Self>) {
        self.right_dock_tab = match panel {
            WorkspaceActivePanel::Inspector => RightDockTab::Inspector,
            WorkspaceActivePanel::ChordDisplay => RightDockTab::ChordDisplay,
            WorkspaceActivePanel::LyricDisplay => RightDockTab::LyricDisplay,
            WorkspaceActivePanel::LyricEditor => RightDockTab::LyricEditor,
            WorkspaceActivePanel::Solfege => RightDockTab::Solfege,
            _ => self.right_dock_tab,
        };
        if self.active_panel == panel {
            return;
        }
        self.active_panel = panel;
        eprintln!("[Workspace] active_panel = {}", panel.label());
        self.notify_bottom_panel_shell(cx);
        self.notify_status_bar(cx);
        cx.notify();
    }

    fn active_panel_for_bottom_tab(tab: BottomTab) -> WorkspaceActivePanel {
        match tab {
            BottomTab::Mixer => WorkspaceActivePanel::Mixer,
            BottomTab::Editor => WorkspaceActivePanel::Editor,
            BottomTab::EffectEditor => WorkspaceActivePanel::EffectEditor,
        }
    }

    /// Whether the docked piano-roll MIDI editor is actually on screen — the
    /// bottom panel is open AND the Editor tab is active. Used to gate keyboard
    /// focus routing: GPUI keeps a closed element's `FocusHandle` reporting
    /// "focused" (orphaned focus), so once the editor tab is hidden we must NOT
    /// treat `self.piano_roll.is_focused()` as the keyboard owner — otherwise the
    /// studio shortcut anchor never reclaims focus and Space/transport shortcuts
    /// silently die until the user clicks a control. See the reclaim guard in
    /// `studio_render`.
    pub(crate) fn docked_midi_editor_visible(&self) -> bool {
        self.panels.bottom_docked && self.active_bottom_tab == BottomTab::Editor
    }

    pub(crate) fn bottom_panel_state(&self) -> BottomPanelState {
        self.bottom_panel_state
    }

    /// Hide the dock from its own header. Same action as the chrome's panel
    /// button, kept on the dock so it is discoverable from the workspace.
    ///
    /// `active_bottom_tab` is deliberately left untouched: hiding the dock is
    /// not a decision about which tab it should show when it comes back.
    pub(crate) fn close_bottom_panel(&mut self, cx: &mut Context<Self>) {
        if !self.panels.bottom_docked {
            return;
        }
        self.panels.bottom_docked = false;
        if matches!(
            self.active_panel,
            WorkspaceActivePanel::Mixer
                | WorkspaceActivePanel::Editor
                | WorkspaceActivePanel::PianoRoll
                | WorkspaceActivePanel::EffectEditor
        ) {
            self.active_panel = WorkspaceActivePanel::Arrangement;
        }
        self.sync_timeline_chrome_metrics(cx);
        self.notify_status_bar(cx);
        cx.notify();
    }

    pub(crate) fn mixer_tree_sidebar_enabled(&self) -> bool {
        self.mixer_view.tree_sidebar_enabled
    }

    pub(crate) fn apply_bottom_panel_resize_start(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bs = &mut self.bottom_panel_state;
        bs.is_resizing = true;
        bs.resize_start_y = f32::from(event.position.y);
        bs.resize_start_height = bs.height_px;
        let window_h: f32 = window.bounds().size.height.into();
        bs.max_height_px = (window_h * 0.70).max(bs.min_height_px + 40.0);
        let _ = cx;
    }

    /// Returns true when height changed enough to refresh the dock shell.
    pub(crate) fn apply_bottom_panel_resize_move(
        &mut self,
        event: &DragMoveEvent<BottomPanelResizeDrag>,
        cx: &mut Context<Self>,
    ) -> bool {
        let bs = &mut self.bottom_panel_state;
        let cur_y: f32 = event.event.position.y.into();
        let delta = bs.resize_start_y - cur_y;
        let new_h = (bs.resize_start_height + delta).clamp(bs.min_height_px, bs.max_height_px);
        if (new_h - bs.height_px).abs() <= 0.5 {
            return false;
        }
        bs.height_px = new_h;
        self.sync_timeline_chrome_metrics(cx);
        let _ = self.timeline.update(cx, |_, cx| cx.notify());
        true
    }

    pub(crate) fn apply_bottom_panel_resize_end(&mut self, cx: &mut Context<Self>) {
        if self.bottom_panel_state.is_resizing {
            self.bottom_panel_state.is_resizing = false;
            self.sync_timeline_chrome_metrics(cx);
            let _ = self.bottom_panel_shell.update(cx, |_, cx| cx.notify());
        }
    }

    pub(crate) fn sync_timeline_chrome_metrics(&self, cx: &mut Context<Self>) {
        // One source of truth: this width flows into
        // `TimelineChromeMetrics.browser_width` → `viewport.panel_origin_x` →
        // every arrangement hit test, so a second copy of the number here is a
        // silent way to break clip hit-testing.
        use crate::components::SIDEBAR_WIDTH;
        const INSPECTOR_WIDTH: f32 = 292.0;
        const STATUS_BAR_HEIGHT: f32 = 22.0;
        let show_browser = self.panels.browser;
        let show_inspector = self.panels.inspector;
        let metrics = crate::components::timeline::TimelineChromeMetrics {
            browser_width: if show_browser { SIDEBAR_WIDTH } else { 0.0 },
            inspector_width: if show_inspector { INSPECTOR_WIDTH } else { 0.0 },
            bottom_panel_height: if self.panels.bottom_docked {
                self.bottom_panel_state.height_px
            } else {
                0.0
            },
            status_bar_height: STATUS_BAR_HEIGHT,
        };
        let project_root = self.project_session.folder_path.clone();
        let _ = self.timeline.update(cx, |timeline, _cx| {
            timeline.set_chrome_metrics(metrics);
            timeline.set_project_root(project_root);
            // The Control Room's destination is an Output Audio Connection, so
            // the device's channel count is validated by the registry rather
            // than mirrored into a separate list of hardware pairs here.
            timeline.state.refresh_output_labels();
        });
    }

    pub(crate) fn notify_bottom_panel_shell(&self, cx: &mut Context<Self>) {
        if self.panels.bottom_docked {
            let _ = self.bottom_panel_shell.update(cx, |_, cx| cx.notify());
        }
    }

    pub(crate) fn set_active_bottom_tab(&mut self, tab: BottomTab, cx: &mut Context<Self>) {
        let tab_changed = self.active_bottom_tab != tab;
        self.active_bottom_tab = tab;
        self.set_active_panel(Self::active_panel_for_bottom_tab(tab), cx);
        if !tab_changed {
            return;
        }
        self.ensure_mixer_tree_defaults_once(cx);
        self.ensure_mixer_tree_ui_hooks(cx.entity().clone(), cx);
        self.notify_bottom_panel_shell(cx);
    }

    pub(crate) fn notify_status_bar(&self, cx: &mut Context<Self>) {
        let _ = self.status_bar.update(cx, |_, cx| cx.notify());
    }

    /// Push footer text into the isolated status entity; skips repaint when unchanged.
    pub(crate) fn notify_status_bar_if_changed(&mut self, cx: &mut Context<Self>) {
        const STATUS_COALESCE: Duration = Duration::from_millis(250);
        crate::perf::count("bottom_panel_timer_tick_count", 1);

        let show_perf = self
            .settings
            .read(cx)
            .current
            .performance
            .show_status_performance_metrics;
        let content = self.status_bar_content(show_perf);
        let sig = status_content_signature(&content);

        if sig == self.engine_sync.last_status_sig {
            return;
        }

        let now = Instant::now();
        let perf_only = show_perf
            && content.perf.is_some()
            && self.engine_sync.last_status_left_audio_sig == left_audio_signature(&content)
            && now.duration_since(self.engine_sync.last_status_poll_at) < STATUS_COALESCE;
        if perf_only {
            return;
        }

        self.engine_sync.last_status_sig = sig;
        self.engine_sync.last_status_left_audio_sig = left_audio_signature(&content);
        self.engine_sync.last_status_poll_at = now;

        let _ = self.status_bar.update(cx, |bar, cx| {
            if bar.apply_content(content) {
                cx.notify();
            }
        });
    }

    pub(crate) fn status_bar_show_perf_metrics(&self, cx: &gpui::App) -> bool {
        self.settings
            .read(cx)
            .current
            .performance
            .show_status_performance_metrics
    }

    pub(crate) fn status_bar_perf_popover_open(&self) -> bool {
        self.overlay.perf_metrics_popover_open
    }

    pub(crate) fn status_bar_background_tasks(&self) -> &crate::components::BackgroundTaskStore {
        &self.background_tasks
    }

    pub(crate) fn toggle_status_bar_background_tasks(&mut self, cx: &mut Context<Self>) {
        self.background_tasks.toggle_panel();
        self.notify_status_bar(cx);
    }

    pub(crate) fn cancel_status_bar_background_task(
        &mut self,
        task_id: &str,
        cx: &mut Context<Self>,
    ) {
        self.background_tasks.cancel(task_id);
        self.notify_status_bar(cx);
    }

    pub(crate) fn toggle_status_bar_perf_popover(&mut self, cx: &mut Context<Self>) {
        self.overlay.perf_metrics_popover_open = !self.overlay.perf_metrics_popover_open;
        self.notify_status_bar(cx);
    }
}

fn left_audio_signature(content: &StatusBarContent) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.left.hash(&mut hasher);
    content.audio.hash(&mut hasher);
    content.listening.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The indicator says "this tab is on screen"; the press has to answer the
    /// same question. Pressing Mixer while the dock shows the Editor must bring
    /// the mixer up, not hide a panel the user was not looking at.
    #[test]
    fn pressing_a_tab_toggle_from_another_tab_shows_it() {
        assert_eq!(
            bottom_tab_toggle_action(true, BottomTab::Editor, BottomTab::Mixer),
            BottomTabToggleAction::ShowTab
        );
    }

    #[test]
    fn pressing_the_tab_already_on_screen_closes_the_dock() {
        assert_eq!(
            bottom_tab_toggle_action(true, BottomTab::Mixer, BottomTab::Mixer),
            BottomTabToggleAction::CloseDock
        );
    }

    #[test]
    fn pressing_a_tab_toggle_while_hidden_opens_it() {
        for tab in [BottomTab::Mixer, BottomTab::Editor] {
            assert_eq!(
                bottom_tab_toggle_action(false, tab, tab),
                BottomTabToggleAction::ShowTab,
                "a hidden dock always opens, whatever tab it was left on"
            );
        }
    }
}
