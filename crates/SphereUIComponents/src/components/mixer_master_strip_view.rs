//! Pinned right-hand strips — Master and Monitor. Isolated invalidation from
//! the channel scroller / tree: this entity repaints on its own display
//! signature alone, never dragging the rest of the mixer with it. Its meters
//! are painted by the mixer's meter layer
//! ([`crate::components::mixer_meter_layer`]), so a level moving does not
//! repaint it at all.

use gpui::{Context, IntoElement, Render, Window};

use crate::components::mixer_meter_layer::SharedMeterLayout;
use crate::components::mixer_panel::{mixer_master_strip_pinned, MixerCallbacks, MixerSplit};
use crate::components::timeline::timeline::Timeline;
use crate::i18n::I18n;

pub struct MixerMasterStripView {
    timeline: gpui::Entity<Timeline>,
    callbacks: MixerCallbacks,
    split: MixerSplit,
    strip_available_px: f32,
    meter_layout: SharedMeterLayout,
    last_display_sig: u64,
    last_structure_key: u64,
}

impl MixerMasterStripView {
    pub fn new(
        timeline: gpui::Entity<Timeline>,
        callbacks: MixerCallbacks,
        split: MixerSplit,
        strip_available_px: f32,
        meter_layout: SharedMeterLayout,
    ) -> Self {
        Self {
            timeline,
            callbacks,
            split,
            strip_available_px,
            meter_layout,
            last_display_sig: u64::MAX,
            last_structure_key: u64::MAX,
        }
    }

    pub fn sync_props(
        &mut self,
        callbacks: MixerCallbacks,
        split: MixerSplit,
        strip_available_px: f32,
    ) -> bool {
        let key = structure_key(&split, strip_available_px);
        let changed = key != self.last_structure_key;
        self.callbacks = callbacks;
        self.split = split;
        self.strip_available_px = strip_available_px;
        if changed {
            self.last_structure_key = key;
            crate::perf::count("mixer_static_snapshot_rebuild_count", 1);
        }
        changed
    }

    /// Poll tick — repaints when something the strips print (not a level)
    /// moved: the Control Room's state, the monitor level.
    pub fn on_display_tick(&mut self, display_sig: u64, cx: &mut Context<Self>) -> bool {
        if display_sig == self.last_display_sig {
            return false;
        }
        self.last_display_sig = display_sig;
        cx.notify();
        true
    }
}

impl Render for MixerMasterStripView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("MixerMasterStrip");
        crate::perf::count("mixer_master_layout_count", 1);
        crate::perf::count("mixer_master_paint_count", 1);

        let timeline = self.timeline.read(cx);
        let mut master = timeline.state.master.clone();
        if let Some(v) = timeline.state.master_volume_preview {
            master.volume = v;
        }
        let monitor = timeline.state.monitor.clone();
        self.last_display_sig = strip_display_signature(&master, &monitor);
        self.meter_layout.borrow_mut().begin_pinned_build();

        let on_master = self.callbacks.on_master_volume_change.clone();
        let i18n = I18n::from_app(cx);
        mixer_master_strip_pinned(
            &master,
            &monitor,
            on_master,
            &self.callbacks,
            &self.split,
            self.strip_available_px,
            Some(&self.meter_layout),
            i18n,
        )
    }
}

fn structure_key(split: &MixerSplit, strip_available_px: f32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let q = |v: f32| (v * 4.0).round() as i64;
    q(split.insert_px).hash(&mut hasher);
    q(split.send_px).hash(&mut hasher);
    split.active_target.hash(&mut hasher);
    q(strip_available_px).hash(&mut hasher);
    hasher.finish()
}

/// What the pinned strips print that a playing project can move without a
/// notify: the Control Room's state and the monitor level. Levels are left
/// out — the meter layer paints them.
fn strip_display_signature(
    master: &crate::components::timeline::timeline_state::MasterBusState,
    monitor: &crate::components::timeline::timeline_state::MonitorBusState,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
    q(master.volume).hash(&mut hasher);
    monitor.listen_active.hash(&mut hasher);
    monitor.mute.hash(&mut hasher);
    monitor.dim.hash(&mut hasher);
    monitor.mono.hash(&mut hasher);
    q(monitor.volume).hash(&mut hasher);
    hasher.finish()
}

pub fn mixer_master_display_signature(
    master: &crate::components::timeline::timeline_state::MasterBusState,
    monitor: &crate::components::timeline::timeline_state::MonitorBusState,
) -> u64 {
    strip_display_signature(master, monitor)
}

/// Quantised meter signature for both pinned strips.
fn strip_meter_signature(
    master: &crate::components::timeline::timeline_state::MasterBusState,
    monitor: &crate::components::timeline::timeline_state::MonitorBusState,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
    q(master.meter_level_l).hash(&mut hasher);
    q(master.meter_level_r).hash(&mut hasher);
    q(master.meter_peak_hold_l).hash(&mut hasher);
    q(master.meter_peak_hold_r).hash(&mut hasher);
    master.meter_clip.hash(&mut hasher);
    q(monitor.meter_level_l).hash(&mut hasher);
    q(monitor.meter_level_r).hash(&mut hasher);
    q(monitor.meter_peak_hold_l).hash(&mut hasher);
    q(monitor.meter_peak_hold_r).hash(&mut hasher);
    monitor.meter_clip.hash(&mut hasher);
    monitor.listen_active.hash(&mut hasher);
    monitor.mute.hash(&mut hasher);
    monitor.dim.hash(&mut hasher);
    monitor.mono.hash(&mut hasher);
    q(monitor.volume).hash(&mut hasher);
    hasher.finish()
}

pub fn mixer_master_meter_signature(
    master: &crate::components::timeline::timeline_state::MasterBusState,
    monitor: &crate::components::timeline::timeline_state::MonitorBusState,
) -> u64 {
    strip_meter_signature(master, monitor)
}
