//! The welcome tour's state on the main window: when it is owed, stepping
//! through it, and remembering that it was seen. The overlay itself is
//! `components::welcome_tour`.

use gpui::{AnyElement, Context, Window};

use crate::components::welcome_tour::{self, TourActions, TourRegions, TourStep, TourVisibility};
use crate::i18n::I18n;

use super::StudioLayout;

impl StudioLayout {
    /// The steps for the regions on screen now.
    fn tour_steps(&self) -> Vec<TourStep> {
        welcome_tour::steps_for(TourVisibility {
            browser: self.panels.browser,
            inspector: self.panels.inspector,
            bottom_panel: self.panels.bottom_docked,
        })
    }

    /// On the main window's first full render: start the tour if it has not
    /// been finished or skipped. Asked once per window.
    pub(super) fn begin_tour_if_owed(&mut self, cx: &mut Context<Self>) {
        if self.tour_checked {
            return;
        }
        self.tour_checked = true;
        if !self.settings.read(cx).current.general.tour_completed {
            self.tour_step = Some(0);
        }
    }

    /// Help ▸ Welcome Tour: from the start, whether or not it was seen.
    pub(super) fn show_tour(&mut self, cx: &mut Context<Self>) {
        self.tour_checked = true;
        self.tour_step = Some(0);
        cx.notify();
    }

    pub(super) fn tour_active(&self) -> bool {
        self.tour_step.is_some()
    }

    pub(super) fn tour_next(&mut self, cx: &mut Context<Self>) {
        let Some(step) = self.tour_step else {
            return;
        };
        if step + 1 >= self.tour_steps().len() {
            self.end_tour(cx);
        } else {
            self.tour_step = Some(step + 1);
            cx.notify();
        }
    }

    pub(super) fn tour_back(&mut self, cx: &mut Context<Self>) {
        if let Some(step) = self.tour_step {
            self.tour_step = Some(step.saturating_sub(1));
            cx.notify();
        }
    }

    /// Finish or skip: the tour goes, and is not shown on its own again.
    pub(super) fn end_tour(&mut self, cx: &mut Context<Self>) {
        self.tour_step = None;
        let settings = self.settings.clone();
        settings.update(cx, |model, cx| {
            if !model.current.general.tour_completed {
                model.update_setting(|schema| schema.general.tour_completed = true, cx);
            }
        });
        cx.notify();
    }

    /// Keys while the tour is up: Escape skips it, Enter and → go on, ← goes
    /// back. Every key stops here, so nothing under the tour acts on it.
    pub(super) fn tour_key(&mut self, key: &str, cx: &mut Context<Self>) {
        match key {
            "escape" => self.end_tour(cx),
            "enter" | "right" => self.tour_next(cx),
            "left" => self.tour_back(cx),
            _ => {}
        }
    }

    /// The overlay for this frame, when the tour is up.
    pub(super) fn render_tour_overlay(
        &mut self,
        window: &Window,
        i18n: I18n,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let step = self.tour_step?;
        let steps = self.tour_steps();
        // A dock closed mid-tour can shorten the list under the current step.
        let step = step.min(steps.len().saturating_sub(1));
        self.tour_step = Some(step);
        let viewport = window.viewport_size();
        let regions = TourRegions {
            window_w: viewport.width.into(),
            window_h: viewport.height.into(),
            main_row: self.main_row_bounds.get(),
            browser_w: if self.panels.browser {
                self.dock_widths.browser
            } else {
                0.0
            },
            inspector_w: if self.panels.inspector {
                self.dock_widths.inspector
            } else {
                0.0
            },
            bottom_h: self
                .panels
                .bottom_docked
                .then(|| self.bottom_panel_state().height_px),
        };
        let owner = cx.entity().downgrade();
        let action = |f: fn(&mut StudioLayout, &mut Context<StudioLayout>)| {
            let owner = owner.clone();
            Box::new(move |_window: &mut Window, cx: &mut gpui::App| {
                let _ = owner.update(cx, |this, cx| f(this, cx));
            }) as Box<dyn Fn(&mut Window, &mut gpui::App)>
        };
        welcome_tour::render_tour(
            &steps,
            step,
            &regions,
            i18n,
            TourActions {
                on_back: action(Self::tour_back),
                on_next: action(Self::tour_next),
                on_skip: action(Self::end_tour),
            },
        )
    }
}
