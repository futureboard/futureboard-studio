//! Status bar entity — footer text updates do not repaint the dock or studio shell.

use std::sync::Arc;

use gpui::{Context, Entity, IntoElement, Render, Window};

use crate::components::status_bar::{status_bar_with_background_tasks, StatusBarContent};
use crate::components::{BackgroundTaskCancelCb, BackgroundTaskToggleCb, PerfMetricsToggleCb};
use crate::layout::StudioLayout;

pub struct StatusBarView {
    owner: Entity<StudioLayout>,
    cached: StatusBarContent,
    content_sig: u64,
}

impl StatusBarView {
    pub fn new(owner: Entity<StudioLayout>) -> Self {
        Self {
            owner,
            cached: StatusBarContent {
                left: String::new(),
                audio: String::new(),
                perf: None,
                listening: None,
            },
            content_sig: u64::MAX,
        }
    }

    /// Apply new footer content; returns true when visible text changed.
    pub fn apply_content(&mut self, content: StatusBarContent) -> bool {
        let sig = status_content_signature(&content);
        if sig == self.content_sig {
            return false;
        }
        self.content_sig = sig;
        self.cached = content;
        crate::perf::count("bottom_panel_status_update_count", 1);
        true
    }
}

impl Render for StatusBarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("BottomPanelFooterStatus");
        crate::perf::count("bottom_panel_footer_layout_count", 1);
        crate::perf::count("bottom_panel_footer_paint_count", 1);

        let owner = self.owner.read(cx);
        let show_perf = owner.status_bar_show_perf_metrics(cx);
        let perf_popover_open = owner.status_bar_perf_popover_open();
        let tasks = owner.status_bar_background_tasks();

        let owner_toggle = self.owner.clone();
        let on_toggle_tasks: BackgroundTaskToggleCb = Arc::new(move |_, _w, cx| {
            let _ = owner_toggle.update(cx, |layout, cx| {
                layout.toggle_status_bar_background_tasks(cx);
            });
        });
        let owner_cancel = self.owner.clone();
        let on_cancel_task: BackgroundTaskCancelCb = Arc::new(move |task_id, _w, cx| {
            let _ = owner_cancel.update(cx, |layout, cx| {
                layout.cancel_status_bar_background_task(&task_id, cx);
            });
        });
        let on_toggle_perf: Option<PerfMetricsToggleCb> = if show_perf {
            let owner = self.owner.clone();
            Some(Arc::new(move |_, _w, cx| {
                let _ = owner.update(cx, |layout, cx| {
                    layout.toggle_status_bar_perf_popover(cx);
                });
            }))
        } else {
            None
        };

        let owner_listening = self.owner.clone();
        let on_open_listening: crate::components::status_bar::ListeningOpenCb =
            Arc::new(move |_w, cx| {
                StudioLayout::defer_update(&owner_listening, cx, |layout, cx| {
                    layout.open_virtual_speaker_window(None, cx);
                });
            });

        status_bar_with_background_tasks(
            self.cached.clone(),
            tasks,
            on_toggle_tasks,
            on_cancel_task,
            perf_popover_open,
            on_toggle_perf,
            Some(on_open_listening),
        )
    }
}

pub fn status_content_signature(content: &StatusBarContent) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.left.hash(&mut hasher);
    content.audio.hash(&mut hasher);
    content.listening.hash(&mut hasher);
    if let Some(perf) = &content.perf {
        perf.pill_label.hash(&mut hasher);
        ((perf.fps * 2.0).round() as i32).hash(&mut hasher);
        ((perf.frame_ms * 10.0).round() as i32).hash(&mut hasher);
    }
    hasher.finish()
}
