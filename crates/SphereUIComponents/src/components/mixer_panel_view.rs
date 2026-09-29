//! Production mixer panel entity — region-isolated invalidation from StudioLayout.
//!
//! Levels are not this entity's to draw. The strips leave a slot where each
//! meter goes and the meter layer ([`crate::components::mixer_meter_layer`])
//! paints them from outside the bottom panel, so a meter tick repaints this
//! panel only when something printed on a strip moves on its own — a fader
//! following volume automation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    div, px, App, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window,
};

use crate::components::mixer_master_strip_view::{
    mixer_master_display_signature, MixerMasterStripView,
};
use crate::components::mixer_meter_layer::{clip_region, ClipRegion, SharedMeterLayout};
use crate::components::mixer_panel::{
    build_mixer_render_snapshot, collect_mixer_render_items, mixer_center_lightweight,
    mixer_strip_scroller, mixer_sub_header, mixer_visible_item_range, MixerRenderItem, MixerSplit,
    MixerSplitAction, MixerSplitDrag, VstiOutputMeterState,
};
use crate::components::mixer_surface::render_mixer_primitives;
use crate::components::mixer_tree_sidebar_view::MixerTreeSidebar;
use crate::components::timeline::timeline::Timeline;
use crate::i18n::I18n;
use crate::layout::StudioLayout;
use crate::theme::Colors;

pub struct MixerPanelView {
    owner: Entity<StudioLayout>,
    timeline: Entity<Timeline>,
    tree_sidebar: Entity<MixerTreeSidebar>,
    master_strip: Entity<MixerMasterStripView>,
    meter_layout: SharedMeterLayout,
    last_structure_key: u64,
    last_master_display_sig: u64,
    last_channel_display_sig: u64,
}

impl MixerPanelView {
    pub fn new(
        owner: Entity<StudioLayout>,
        timeline: Entity<Timeline>,
        tree_sidebar: Entity<MixerTreeSidebar>,
        master_strip: Entity<MixerMasterStripView>,
        meter_layout: SharedMeterLayout,
    ) -> Self {
        Self {
            owner,
            timeline,
            tree_sidebar,
            master_strip,
            meter_layout,
            last_structure_key: u64::MAX,
            last_master_display_sig: u64::MAX,
            last_channel_display_sig: u64::MAX,
        }
    }

    /// Audio poll tick. The levels are the meter layer's; this repaints the
    /// strips only when a playing project moves something else they print.
    pub fn on_meter_tick(&mut self, cx: &mut Context<Self>) {
        let (master_sig, channel_sig) = {
            let state = &self.timeline.read(cx).state;
            (
                mixer_master_display_signature(&state.master, &state.monitor),
                channel_display_signature(state),
            )
        };
        if master_sig != self.last_master_display_sig {
            self.last_master_display_sig = master_sig;
            let _ = self.master_strip.update(cx, |master, cx| {
                master.on_display_tick(master_sig, cx);
            });
        }
        if channel_sig != self.last_channel_display_sig {
            self.last_channel_display_sig = channel_sig;
            crate::perf::count("mixer_automation_repaint_count", 1);
            cx.notify();
        }
    }

    fn read_view_state(&self, cx: &App) -> MixerPanelViewState {
        let _scope = crate::perf::PerfScope::enter("MixerViewStateClone");
        let owner = self.owner.read(cx);
        let chrome = owner.docked_mixer_panel_state(cx);
        let timeline = self.timeline.read(cx);
        let collapsed =
            crate::components::timeline::timeline_state::collapsed_vsti_output_group_keys_from_tracks(
                &timeline.state.tracks,
            );
        let hidden = timeline.state.mixer_tree.hidden_channel_ids.clone();
        let render_items = collect_mixer_render_items(&timeline.state.tracks, &collapsed, &hidden);
        let strip_count = render_items.len();
        let visible_range =
            mixer_visible_item_range(strip_count, chrome.scroll_x, chrome.viewport_width);
        let mut detailed_track_indices = HashSet::new();
        for item in &render_items[visible_range] {
            match *item {
                MixerRenderItem::Track { track_index } => {
                    detailed_track_indices.insert(track_index);
                }
                MixerRenderItem::VstiOutput {
                    parent_index,
                    child_index,
                    ..
                } => {
                    detailed_track_indices.insert(parent_index);
                    detailed_track_indices.insert(child_index);
                }
            }
        }

        // Mixer strips never inspect arrangement clips. A full TrackState clone
        // also clones every MIDI note/controller vector, which made a simple
        // selection repaint take seconds in large projects.
        let mut tracks: Vec<_> = timeline
            .state
            .tracks
            .iter()
            .enumerate()
            .map(|(track_index, track)| {
                if detailed_track_indices.contains(&track_index) {
                    crate::layout::clone_track_for_mixer(track)
                } else {
                    crate::layout::clone_track_for_mixer_summary(track)
                }
            })
            .collect();
        let mut master = timeline.state.master.clone();
        timeline
            .state
            .apply_volume_previews_to_snapshot(&mut tracks, &mut master);

        MixerPanelViewState {
            tracks,
            master,
            selected_track_id: timeline.state.selection.selected_track_id.clone(),
            selected_track_ids: timeline.state.selection.selected_track_ids.clone(),
            collapsed,
            hidden,
            vsti_output_meters: chrome.vsti_output_meters.clone(),
            scroll_x: chrome.scroll_x,
            viewport_width: chrome.viewport_width,
            strip_available_px: chrome.strip_available_px,
            body_scrolls: chrome.body_scrolls,
            strip_count,
            track_count: timeline.state.tracks.len(),
            tree_enabled: chrome.tree_sidebar_enabled,
            gpu_decor: crate::components::mixer_surface::mixer_gpu_primitives_active(),
        }
    }

    fn structure_key(state: &MixerPanelViewState, split: &MixerSplit) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let q = |v: f32| (v * 4.0).round() as i64;
        state.strip_count.hash(&mut hasher);
        state.track_count.hash(&mut hasher);
        state.selected_track_id.as_deref().hash(&mut hasher);
        state.selected_track_ids.hash(&mut hasher);
        q(state.scroll_x).hash(&mut hasher);
        q(state.viewport_width).hash(&mut hasher);
        q(state.strip_available_px).hash(&mut hasher);
        state.body_scrolls.hash(&mut hasher);
        q(split.insert_px).hash(&mut hasher);
        q(split.send_px).hash(&mut hasher);
        split.active_target.hash(&mut hasher);
        state.hidden.len().hash(&mut hasher);
        state.collapsed.len().hash(&mut hasher);
        hasher.finish()
    }
}

struct MixerPanelViewState {
    tracks: Vec<crate::components::timeline::timeline_state::TrackState>,
    master: crate::components::timeline::timeline_state::MasterBusState,
    selected_track_id: Option<String>,
    selected_track_ids: Vec<String>,
    collapsed: HashSet<String>,
    hidden: HashSet<String>,
    vsti_output_meters: HashMap<String, VstiOutputMeterState>,
    scroll_x: f32,
    viewport_width: f32,
    strip_available_px: f32,
    body_scrolls: bool,
    strip_count: usize,
    track_count: usize,
    tree_enabled: bool,
    gpu_decor: bool,
}

impl Render for MixerPanelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("MixerPanel");
        crate::perf::count("mixer_root_layout_count", 1);
        crate::perf::count("mixer_root_paint_count", 1);

        let i18n = I18n::from_app(cx);
        let owner_entity = self.owner.clone();
        let callbacks = self
            .owner
            .read(cx)
            .build_mixer_callbacks(owner_entity.clone(), cx);
        let split = build_mixer_split(&self.owner, cx);
        let state = self.read_view_state(cx);
        // The strips are rebuilding; they record their meter slots again as
        // they lay out, and a strip gone from view must not leave its old one.
        self.meter_layout.borrow_mut().begin_strips_build();
        self.last_channel_display_sig = channel_display_signature(&self.timeline.read(cx).state);

        let structure_key = Self::structure_key(&state, &split);
        if structure_key != self.last_structure_key {
            self.last_structure_key = structure_key;
            crate::perf::count("mixer_static_snapshot_rebuild_count", 1);
        }

        let _ = self.master_strip.update(cx, |master, _cx| {
            master.sync_props(callbacks.clone(), split.clone(), state.strip_available_px);
        });

        let panel_entity = cx.entity();
        let on_scroll = build_scroll_handler(owner_entity.clone(), panel_entity);
        let split_for_move = split.clone();
        let split_for_end = split.clone();

        // A panel shorter than one strip lays the strips out at their full
        // height and scrolls them, rather than clipping the fader and the name
        // plate off their bottom.
        let body_scrolls = state.body_scrolls;
        let strip_h = state.strip_available_px;
        let fill = move |row: gpui::Div| {
            if body_scrolls {
                row.flex_none().h(px(strip_h))
            } else {
                row.flex_1().min_h_0()
            }
        };

        let mut channel_row = if state.strip_count == 0 {
            crate::perf::count("mixer_center_paint_count", 1);
            fill(div().flex().flex_row())
                .child(mixer_center_lightweight(
                    state.viewport_width,
                    state.strip_available_px,
                ))
                .child(div().w(px(1.0)).h_full().bg(Colors::border_default()))
                .child(self.master_strip.clone())
        } else {
            let strip_row = mixer_strip_scroller(
                &state.tracks,
                state.selected_track_id.as_deref(),
                &state.selected_track_ids,
                callbacks.clone(),
                &state.collapsed,
                &state.hidden,
                &state.vsti_output_meters,
                state.scroll_x,
                state.viewport_width,
                state.strip_available_px,
                state.body_scrolls,
                &split,
                on_scroll,
                state.gpu_decor,
                Some(&self.meter_layout),
                i18n,
            );
            fill(div().flex().flex_row())
                .child(strip_row)
                .child(div().w(px(1.0)).h_full().bg(Colors::border_default()))
                .child(self.master_strip.clone())
        };

        // In GPU-decoration mode the strip elements intentionally omit their
        // background, accent, and separator. Compose the same primitive layer
        // used by the detached Mixer Window behind the docked strip row.
        if state.gpu_decor && state.strip_count > 0 {
            let snapshot = build_mixer_render_snapshot(
                &state.tracks,
                &state.collapsed,
                &state.hidden,
                state.selected_track_id.as_deref(),
                &state.selected_track_ids,
                state.scroll_x,
                state.viewport_width,
                state.strip_available_px,
            );
            let primitives = render_mixer_primitives(&snapshot);
            channel_row = fill(div().relative())
                .child(primitives)
                .child(channel_row.size_full());
        }

        let channel_row = if body_scrolls {
            div()
                .id("mixer-body-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(channel_row)
                .into_any_element()
        } else {
            channel_row.into_any_element()
        };
        // The region the meter layer may paint in: the strips and the pinned
        // pair as far as they are on screen, never the tree sidebar or the
        // header. It sits beside the scroller, not in it, so it stays the
        // visible part when the strips scroll.
        let channel_row = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(channel_row)
            .child(clip_region(&self.meter_layout, ClipRegion::Body));
        let body = if state.tree_enabled {
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h_0()
                .child(self.tree_sidebar.clone())
                .child(channel_row)
        } else {
            channel_row
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Colors::surface_window())
            .on_drag_move::<MixerSplitDrag>(move |event, w, cx| {
                let y: f32 = event.event.position.y.into();
                (split_for_move.on_action)(MixerSplitAction::ResizeMove(y), w, cx);
            })
            .on_mouse_up(gpui::MouseButton::Left, move |_e, w, cx| {
                (split_for_end.on_action)(MixerSplitAction::ResizeEnd, w, cx);
            })
            .child(mixer_sub_header(
                state.track_count,
                state.tracks.iter().filter(|track| track.solo).count(),
                Some({
                    let timeline = self.timeline.clone();
                    std::sync::Arc::new(move |_window: &mut Window, cx: &mut App| {
                        let _ = timeline.update(cx, |timeline, cx| timeline.clear_all_solos(cx));
                    })
                }),
                i18n,
            ))
            .child(body)
    }
}

fn build_mixer_split(owner: &Entity<StudioLayout>, cx: &App) -> MixerSplit {
    let owner_entity = owner.clone();
    let on_action: Arc<dyn Fn(MixerSplitAction, &mut Window, &mut App) + 'static> =
        Arc::new(move |action, _w, cx| {
            let _ = owner_entity.update(cx, |layout, cx| {
                layout.apply_mixer_split_action(action, cx);
            });
        });
    let layout = owner.read(cx);
    crate::components::mixer_panel::MixerSplit {
        insert_px: layout.mixer_insert_section_px(),
        send_px: layout.mixer_send_section_px(),
        active_target: layout.mixer_split_active_target(),
        on_action,
    }
}

fn build_scroll_handler(
    owner: Entity<StudioLayout>,
    panel: Entity<MixerPanelView>,
) -> Arc<dyn Fn(f32, &mut Window, &mut App) + 'static> {
    Arc::new(move |new_x, _w, cx| {
        let _ = owner.update(cx, |layout, cx| {
            if layout.set_mixer_scroll_x(new_x, cx) {
                layout.push_mixer_snapshot_to_window(cx);
                let _ = panel.update(cx, |_, cx| cx.notify());
            }
        });
    })
}

/// What a playing project moves on the channel strips without anyone
/// notifying the mixer: a fader following its volume automation. Levels are
/// not in it — the meter layer paints those.
fn channel_display_signature(
    state: &crate::components::timeline::timeline_state::TimelineState,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for track in &state.tracks {
        if track.has_active_volume_automation() {
            ((state.display_track_volume(track) * 1000.0).round() as i32).hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Docked mixer shell: the panel entity is the MixerPanelRoot and owns its body
/// children, including the tree sidebar when enabled.
pub fn docked_mixer_shell(mixer_panel: Entity<MixerPanelView>) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .flex_1()
        .min_h_0()
        .size_full()
        .child(mixer_panel)
}
