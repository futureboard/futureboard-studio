//! The arrangement's per-frame visuals — the playhead and the track-header
//! meters — painted from outside `Timeline`.
//!
//! Both move at the display rate while the transport runs. They used to be
//! entities *inside* the arrangement, and GPUI marks every ancestor of a
//! notified view dirty: each playhead or meter tick re-ran all of
//! `Timeline::render` — the ~60 gesture callbacks, the ruler, the conductor
//! lanes, every visible header and automation lane — to move a line and a few
//! pixels of green.
//!
//! So `Timeline` now only *records* where they go, while it lays itself out:
//! its own bounds, each visible header's meter slot, the header area the meters
//! are clipped to, and the floating controls the playhead passes under. The
//! studio root renders the two layers here beside a cached `Timeline` (the
//! same arrangement as the docked mixer's `MixerMeterOverlay`), so a tick
//! re-renders the root and these layers and reuses the arrangement as drawn.
//! What was recorded stays valid for exactly as long as that reuse does; any
//! re-render of `Timeline` records it again.
//!
//! Everything is read in *paint*: a re-rendering `Timeline` records during its
//! prepaint, which runs after these layers render but before they paint.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use gpui::{
    canvas, div, fill, point, px, size, Bounds, ContentMask, Context, IntoElement, ParentElement,
    Pixels, Render, Styled, WeakEntity, Window,
};

use crate::components::timeline::playhead::PlayheadFrameCell;
use crate::components::timeline::vu_meter::{
    paint_compact_meter, COMPACT_METER_HEIGHT, COMPACT_METER_WIDTH,
};
use crate::components::timeline::Timeline;
use crate::theme::Colors;

/// What the arrangement's last layout recorded for the layers.
#[derive(Default)]
pub struct ArrangementOverlayLayout {
    /// The `Timeline` root, in window space.
    timeline_bounds: Option<Bounds<Pixels>>,
    /// Visible header meters: track id → (track index when recorded, bounds).
    meter_slots: HashMap<String, (usize, Bounds<Pixels>)>,
    /// The track rows' scroll area. A header scrolled half under the conductor
    /// lanes must not paint its meter over them.
    meter_clip: Option<Bounds<Pixels>>,
    /// Arrangement controls drawn above the playhead (the floating tools bar,
    /// the zoom pill). The line is interrupted where it crosses them, as it
    /// was when it was drawn underneath them inside the arrangement.
    occluders: HashMap<&'static str, Bounds<Pixels>>,
}

impl ArrangementOverlayLayout {
    /// `Timeline` is re-rendering: forget what its last layout recorded. The
    /// bounds are recorded on every layout, so they are kept.
    pub(crate) fn begin_build(&mut self) {
        self.meter_slots.clear();
        self.meter_clip = None;
        self.occluders.clear();
    }

    /// Quantised identity of the levels the visible meters would show. The
    /// bars are a handful of pixels tall, so anything finer than 1/255 of full
    /// scale draws the same meters.
    pub(crate) fn meter_signature(
        &self,
        tracks: &[crate::components::timeline::timeline_state::TrackState],
    ) -> u64 {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u32;
        let mut sig = self.meter_slots.len() as u64;
        for (id, (index, _)) in &self.meter_slots {
            let Some(track) = track_at(tracks, id, *index) else {
                continue;
            };
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            id.hash(&mut hasher);
            q(track.meter_level_l).hash(&mut hasher);
            q(track.meter_level_r).hash(&mut hasher);
            // Order-independent: the slots are a map.
            sig = sig.wrapping_add(hasher.finish());
        }
        sig
    }
}

pub type SharedArrangementOverlay = Rc<RefCell<ArrangementOverlayLayout>>;

/// The track at `index` if it is still `id`, else a search: a reordered track
/// list must not paint another track's level.
fn track_at<'a>(
    tracks: &'a [crate::components::timeline::timeline_state::TrackState],
    id: &str,
    index: usize,
) -> Option<&'a crate::components::timeline::timeline_state::TrackState> {
    tracks
        .get(index)
        .filter(|track| track.id == id)
        .or_else(|| tracks.iter().find(|track| track.id == id))
}

/// An invisible, input-free child filling its parent; `record` gets the
/// parent's window bounds during prepaint.
fn probe(record: impl FnOnce(Bounds<Pixels>) + 'static) -> impl IntoElement {
    canvas(
        move |bounds, _window, _cx| record(bounds),
        |_, _: (), _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Records the `Timeline` root's bounds. A child of that root.
pub(crate) fn timeline_bounds_probe(layout: &SharedArrangementOverlay) -> impl IntoElement {
    let layout = layout.clone();
    probe(move |bounds| layout.borrow_mut().timeline_bounds = Some(bounds))
}

/// Records the track rows' scroll area as the meters' clip. A child of it.
pub(crate) fn meter_clip_probe(layout: &SharedArrangementOverlay) -> impl IntoElement {
    let layout = layout.clone();
    probe(move |bounds| layout.borrow_mut().meter_clip = Some(bounds))
}

/// Records its parent as a control the playhead passes under.
pub(crate) fn occluder_probe(
    layout: &SharedArrangementOverlay,
    key: &'static str,
) -> impl IntoElement {
    let layout = layout.clone();
    probe(move |bounds| {
        layout.borrow_mut().occluders.insert(key, bounds);
    })
}

/// Where a header's compact meter goes: its size, nothing painted. Its bounds
/// are recorded for [`ArrangementMeterOverlay`].
pub(crate) fn track_meter_slot(
    layout: &SharedArrangementOverlay,
    track_id: String,
    index: usize,
) -> impl IntoElement {
    let layout = layout.clone();
    div()
        .flex_none()
        .w(px(COMPACT_METER_WIDTH))
        .h(px(COMPACT_METER_HEIGHT))
        .child(
            canvas(
                move |bounds, _window, _cx| {
                    layout
                        .borrow_mut()
                        .meter_slots
                        .insert(track_id, (index, bounds));
                },
                |_, _: (), _, _| {},
            )
            .size_full(),
        )
}

/// The header meters, painted with the current levels.
pub struct ArrangementMeterOverlay {
    timeline: WeakEntity<Timeline>,
    layout: SharedArrangementOverlay,
    /// Not zero: an overlay that has never been fed must paint once, and a
    /// real signature of 0 is no meters at all.
    last_sig: u64,
}

impl ArrangementMeterOverlay {
    pub(crate) fn new(timeline: WeakEntity<Timeline>, layout: SharedArrangementOverlay) -> Self {
        Self {
            timeline,
            layout,
            last_sig: u64::MAX,
        }
    }

    /// One meter poll tick. Repaints only when a drawn pixel would change.
    pub(crate) fn apply_signature(&mut self, sig: u64, cx: &mut Context<Self>) -> bool {
        if sig == self.last_sig {
            return false;
        }
        self.last_sig = sig;
        cx.notify();
        true
    }
}

impl Render for ArrangementMeterOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::count("track_meter_paint_count", 1);
        let timeline = self.timeline.clone();
        let layout = self.layout.clone();
        canvas(
            |_bounds, _window, _cx| (),
            move |_bounds, (), window, cx| {
                let Some(timeline) = timeline.upgrade() else {
                    return;
                };
                let layout = layout.borrow();
                let Some(clip) = layout.meter_clip else {
                    return;
                };
                let tracks = &timeline.read(cx).state.tracks;
                let meters: Vec<(Bounds<Pixels>, f32, f32)> = layout
                    .meter_slots
                    .iter()
                    .filter(|(_, (_, bounds))| bounds.intersects(&clip))
                    .filter_map(|(id, (index, bounds))| {
                        let track = track_at(tracks, id, *index)?;
                        Some((*bounds, track.meter_level_l, track.meter_level_r))
                    })
                    .collect();
                window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                    for (bounds, level_l, level_r) in meters {
                        paint_compact_meter(bounds, level_l, level_r, window);
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

/// An empty view inside the arrangement, there only to be notified.
///
/// GPUI re-renders every ancestor of a notified view, and `Timeline`'s
/// observers — every cached clip lane and the grid among them — hear only a
/// `Timeline` notify. Notifying this child therefore re-renders the
/// arrangement's own inline UI (the track headers, with their faders) and
/// leaves the lanes and the grid as they were drawn: what a fader following
/// volume automation needs, at the display rate, and nothing more.
pub struct InlineRefresh;

impl Render for InlineRefresh {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// The playhead: its head in the ruler, its line through the conductor lanes
/// and the track rows.
pub struct PlayheadOverlay {
    frame: PlayheadFrameCell,
    layout: SharedArrangementOverlay,
    /// y of the first row under the ruler, from the `Timeline` top. The head is
    /// clipped above it and the line starts at it.
    ruler_height: f32,
    /// Left edge of the lane column — the track headers keep their own strip.
    header_width: f32,
}

impl PlayheadOverlay {
    pub fn new(
        frame: PlayheadFrameCell,
        layout: SharedArrangementOverlay,
        ruler_height: f32,
        header_width: f32,
    ) -> Self {
        Self {
            frame,
            layout,
            ruler_height,
            header_width,
        }
    }
}

impl Render for PlayheadOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("PlayheadOverlay");
        crate::perf::count("playhead_overlay_paint_count", 1);
        let frame = self.frame.clone();
        let layout = self.layout.clone();
        let (ruler_height, header_width) = (self.ruler_height, self.header_width);
        canvas(
            |_bounds, _window, _cx| (),
            move |_bounds, (), window, cx| {
                let layout = layout.borrow();
                let Some(timeline) = layout.timeline_bounds else {
                    return;
                };
                // Read here, not in render: a `Timeline` re-rendering this
                // frame recomputes x for its new scroll and zoom first.
                let x = frame.get().x;
                let lane_left = f32::from(timeline.origin.x) + header_width;
                let lane_w = f32::from(timeline.right()) - lane_left;
                let top = f32::from(timeline.origin.y);
                let body_top = top + ruler_height;
                let bottom = f32::from(timeline.bottom());
                if lane_w <= 0.0 || x < -2.0 || x > lane_w + 2.0 {
                    return;
                }
                let color = Colors::timeline_playhead();
                let line_x = lane_left + x;

                let head = Bounds::new(
                    point(px(lane_left), px(top)),
                    size(px(lane_w), px(ruler_height.min(bottom - top).max(0.0))),
                );
                window.with_content_mask(Some(ContentMask { bounds: head }), |window| {
                    // Kept inside the ruler strip, as the head always was.
                    let head_bounds = Bounds::new(
                        point(px(line_x - 5.5), px(top + 2.0)),
                        size(px(12.0), px(12.0)),
                    );
                    let _ = window.paint_svg(
                        head_bounds,
                        crate::assets::ICON_PLAYHEAD_HANDLE_PATH.into(),
                        None,
                        gpui::TransformationMatrix::unit(),
                        color.into(),
                        cx,
                    );
                });

                if bottom <= body_top {
                    return;
                }
                let body = Bounds::new(
                    point(px(lane_left), px(body_top)),
                    size(px(lane_w), px(bottom - body_top)),
                );
                let occluders: Vec<(f32, f32, f32, f32)> = layout
                    .occluders
                    .values()
                    .map(|b| {
                        (
                            f32::from(b.left()),
                            f32::from(b.right()),
                            f32::from(b.top()),
                            f32::from(b.bottom()),
                        )
                    })
                    .collect();
                window.with_content_mask(Some(ContentMask { bounds: body }), |window| {
                    window.paint_layer(body, |window| {
                        for (from, to) in visible_spans(line_x, body_top, bottom, &occluders) {
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(px(line_x), px(from)),
                                    size(px(1.0), px(to - from)),
                                ),
                                color,
                            ));
                        }
                    });
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

/// The parts of a 1 px vertical line at `x` from `top` to `bottom` that no
/// occluder `(left, right, top, bottom)` covers.
fn visible_spans(
    x: f32,
    top: f32,
    bottom: f32,
    occluders: &[(f32, f32, f32, f32)],
) -> Vec<(f32, f32)> {
    let mut spans = vec![(top, bottom)];
    for &(o_left, o_right, o_top, o_bottom) in occluders {
        if x + 1.0 <= o_left || x >= o_right {
            continue;
        }
        spans = spans
            .into_iter()
            .flat_map(|(from, to)| [(from, to.min(o_top)), (from.max(o_bottom), to)])
            .filter(|(from, to)| to > from)
            .collect();
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_is_whole_where_nothing_covers_it() {
        assert_eq!(visible_spans(50.0, 0.0, 100.0, &[]), vec![(0.0, 100.0)]);
        // Beside the occluder, not under it.
        let beside = [(60.0, 80.0, 20.0, 40.0)];
        assert_eq!(visible_spans(50.0, 0.0, 100.0, &beside), vec![(0.0, 100.0)]);
    }

    #[test]
    fn the_line_breaks_where_a_control_sits_over_it() {
        let toolbar = [(40.0, 80.0, 20.0, 40.0)];
        assert_eq!(
            visible_spans(50.0, 0.0, 100.0, &toolbar),
            vec![(0.0, 20.0), (40.0, 100.0)]
        );
        // A second one further down, and one reaching past the bottom.
        let both = [(40.0, 80.0, 20.0, 40.0), (45.0, 55.0, 90.0, 120.0)];
        assert_eq!(
            visible_spans(50.0, 0.0, 100.0, &both),
            vec![(0.0, 20.0), (40.0, 90.0)]
        );
    }

    fn one_track_layout(
        level_l: f32,
        level_r: f32,
    ) -> (
        ArrangementOverlayLayout,
        Vec<crate::components::timeline::timeline_state::TrackState>,
    ) {
        use crate::components::timeline::timeline_state::{
            CreateTrackOptions, InputMonitorMode, TimelineState, TrackType,
        };
        let mut state = TimelineState::default();
        state.tracks.clear();
        let id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name: "Audio".to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        state.tracks[0].meter_level_l = level_l;
        state.tracks[0].meter_level_r = level_r;
        let mut layout = ArrangementOverlayLayout::default();
        layout.meter_slots.insert(id, (0, Bounds::default()));
        (layout, state.tracks)
    }

    fn sig(level_l: f32, level_r: f32) -> u64 {
        let (layout, tracks) = one_track_layout(level_l, level_r);
        layout.meter_signature(&tracks)
    }

    /// The gate is what keeps meter ticks cheap: the poll runs at display
    /// refresh, and most ticks move a level by less than a drawn pixel.
    #[test]
    fn sub_pixel_drift_is_the_same_meter() {
        assert_eq!(sig(0.5000, 0.2500), sig(0.5001, 0.2501));
    }

    #[test]
    fn a_visible_step_or_the_other_channel_is_a_different_meter() {
        assert_ne!(sig(0.10, 0.10), sig(0.90, 0.10));
        assert_ne!(sig(1.0, 0.0), sig(0.0, 1.0));
    }

    /// Levels arrive from the engine unclamped; the meter is drawn clamped.
    #[test]
    fn out_of_range_levels_clamp_like_the_drawing_does() {
        assert_eq!(sig(1.5, -0.5), sig(1.0, 0.0));
    }

    /// Only visible meters count: a level on a track with no slot repaints
    /// nothing.
    #[test]
    fn a_track_without_a_slot_does_not_move_the_signature() {
        let (mut layout, tracks) = one_track_layout(0.5, 0.5);
        layout.meter_slots.clear();
        let mut louder = tracks.clone();
        louder[0].meter_level_l = 1.0;
        assert_eq!(
            layout.meter_signature(&tracks),
            layout.meter_signature(&louder)
        );
    }

    #[test]
    fn a_rebuild_forgets_the_last_layouts_slots_but_not_the_bounds() {
        let mut layout = ArrangementOverlayLayout::default();
        layout.timeline_bounds = Some(Bounds::default());
        layout
            .meter_slots
            .insert("t1".into(), (0, Bounds::default()));
        layout.meter_clip = Some(Bounds::default());
        layout.occluders.insert("toolbar", Bounds::default());
        layout.begin_build();
        assert!(layout.meter_slots.is_empty());
        assert!(layout.meter_clip.is_none());
        assert!(layout.occluders.is_empty());
        assert!(layout.timeline_bounds.is_some());
    }
}
