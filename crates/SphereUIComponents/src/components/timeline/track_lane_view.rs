//! A track's clip lane as its own cached view.
//!
//! The lane is where the arrangement's real cost lives: clips, waveform
//! canvases, MIDI note previews, one set per visible track. It used to be
//! rebuilt on every timeline repaint — and during playback that is every
//! display frame, because GPUI's `mark_view_dirty` walks a notified view's
//! *ancestors*: the playhead overlay and the per-track meters both sit under
//! `Timeline`, so each of their ticks dirtied the whole arrangement.
//!
//! Hosting each lane in its own view makes it a **sibling** of the header (with
//! its meter) and of the playhead overlay, which is the one relationship
//! `AnyView::cached` can isolate. A meter tick still rebuilds the header it
//! lives in; the clips beside it are reused.
//!
//! The lane owns no state. `Timeline::render` publishes a
//! [`LaneFrameContext`] — the gesture snapshot, the row layout and the lane
//! callbacks it already builds every frame — and the view renders through it,
//! so there is no second copy of the arrangement to keep in sync.

use std::sync::Arc;

use gpui::{div, px, Context, Entity, IntoElement, Render, Styled, WeakEntity, Window};

use crate::components::timeline::audio_clip::{
    AudioClipCutCb, AudioClipProcessCommitCb, AudioClipProcessPreviewCb,
};
use crate::components::timeline::timeline_state::{TimelineGestureContext, TrackRowLayout};
use crate::components::timeline::Timeline;

/// Lane views keyed by track id. Built and pruned by `Timeline::render`
/// alongside the meter entities.
pub type TrackLaneViews = std::collections::HashMap<String, Entity<TrackLaneView>>;

/// What a lane needs from the frame `Timeline::render` is building.
///
/// Every field is something that render already produced for the inline path;
/// publishing them costs one `Rc` and a handful of `Arc` clones, and lets a
/// dirty lane rebuild itself without re-deriving the frame.
pub struct LaneFrameContext {
    pub gesture: std::rc::Rc<TimelineGestureContext>,
    pub row_layout: std::rc::Rc<TrackRowLayout>,
    pub on_select_track: Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>,
    pub on_select_clip:
        Arc<dyn Fn(&(String, bool, bool), &mut gpui::Window, &mut gpui::App) + 'static>,
    pub on_add_clip:
        Arc<dyn Fn(&(String, f32, u32, bool), &mut gpui::Window, &mut gpui::App) + 'static>,
    pub on_track_context_menu:
        Option<Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>>,
    pub on_clip_context_menu:
        Option<Arc<dyn Fn(&(String, f32, f32), &mut gpui::Window, &mut gpui::App) + 'static>>,
    pub on_open_editor: Option<Arc<dyn Fn(&mut gpui::Window, &mut gpui::App) + 'static>>,
    pub on_range_start: Option<crate::components::timeline::track_lane::MarqueePressCb>,
    pub on_erase_start: Option<Arc<dyn Fn(&f32, &mut gpui::Window, &mut gpui::App) + 'static>>,
    pub on_erase_clip: Option<Arc<dyn Fn(&String, &mut gpui::Window, &mut gpui::App) + 'static>>,
    pub on_cut_clip: Option<AudioClipCutCb>,
    pub on_audio_clip_process_preview: AudioClipProcessPreviewCb,
    pub on_audio_clip_process_commit: AudioClipProcessCommitCb,
    pub on_crossfade: Option<crate::components::timeline::crossfade_overlay::CrossfadeGestureCb>,
}

pub struct TrackLaneView {
    timeline: WeakEntity<Timeline>,
    track_id: String,
    /// Clips, selection, zoom, scroll and tools are all `Timeline` state, so a
    /// `Timeline` notify is what a cached lane has to hear. Playhead and meter
    /// ticks deliberately do not notify `Timeline`, which is what keeps them
    /// out of the lane.
    _observer: gpui::Subscription,
}

impl TrackLaneView {
    pub fn new(timeline: &Entity<Timeline>, track_id: String, cx: &mut Context<Self>) -> Self {
        let _observer = cx.observe(timeline, |_, _, cx| cx.notify());
        Self {
            timeline: timeline.downgrade(),
            track_id,
            _observer,
        }
    }

    /// Layout-affecting half of `track_lane`'s root. A row-height change moves
    /// the cached bounds too, so the cache invalidates itself on a track
    /// resize even before the resize notifies.
    pub fn cached_style(row_height: f32) -> gpui::StyleRefinement {
        gpui::StyleRefinement::default()
            .flex_1()
            .h(px(row_height))
            .relative()
            .overflow_hidden()
    }
}

impl Render for TrackLaneView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(timeline) = self.timeline.upgrade() else {
            return div().into_any_element();
        };
        timeline.read(cx).render_track_lane(&self.track_id)
    }
}
