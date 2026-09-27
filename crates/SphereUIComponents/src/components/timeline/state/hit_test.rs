use super::*;

pub type ClipId = String;
pub type MarkerId = String;
pub type AutomationLaneId = String;

#[derive(Clone, Debug, PartialEq)]
pub enum ArrangementHitTarget {
    EmptyArrangement {
        timeline_beat: f64,
        track_id: Option<TrackId>,
    },
    TrackHeader {
        track_id: TrackId,
    },
    TrackLane {
        track_id: TrackId,
        timeline_beat: f64,
    },
    AudioClip {
        track_id: TrackId,
        clip_id: ClipId,
        timeline_beat: f64,
        local_beat: f64,
    },
    MidiClip {
        track_id: TrackId,
        clip_id: ClipId,
        timeline_beat: f64,
        local_beat: f64,
    },
    VideoClip {
        track_id: TrackId,
        clip_id: ClipId,
        timeline_beat: f64,
        local_beat: f64,
    },
    Ruler {
        timeline_beat: f64,
    },
    Marker {
        marker_id: MarkerId,
        timeline_beat: f64,
    },
    AutomationLane {
        track_id: TrackId,
        lane_id: AutomationLaneId,
        timeline_beat: f64,
    },
}

impl ArrangementHitTarget {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::EmptyArrangement { .. } => "EmptyArrangement",
            Self::TrackHeader { .. } => "TrackHeader",
            Self::TrackLane { .. } => "TrackLane",
            Self::AudioClip { .. } => "AudioClip",
            Self::MidiClip { .. } => "MidiClip",
            Self::VideoClip { .. } => "VideoClip",
            Self::Ruler { .. } => "Ruler",
            Self::Marker { .. } => "Marker",
            Self::AutomationLane { .. } => "AutomationLane",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArrangementCoordinateContext {
    pub panel_origin_px: gpui::Point<gpui::Pixels>,
    pub viewport_origin_px: gpui::Point<gpui::Pixels>,
    pub scroll_x_px: f32,
    pub scroll_y_px: f32,
    pub zoom_px_per_beat: f32,
    pub ruler_height_px: f32,
    pub track_header_width_px: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ArrangementLocalPoint {
    pub panel_x: f32,
    pub panel_y: f32,
    pub viewport_x: f32,
    pub viewport_y: f32,
    pub content_x: f32,
    pub content_y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ArrangementHitTestResult {
    pub target: ArrangementHitTarget,
    pub local: ArrangementLocalPoint,
    pub z_priority: u8,
}

pub fn screen_to_arrangement_local(
    screen: gpui::Point<gpui::Pixels>,
    ctx: &ArrangementCoordinateContext,
) -> ArrangementLocalPoint {
    let sx: f32 = screen.x.into();
    let sy: f32 = screen.y.into();
    let panel_origin_x: f32 = ctx.panel_origin_px.x.into();
    let panel_origin_y: f32 = ctx.panel_origin_px.y.into();
    let viewport_origin_x: f32 = ctx.viewport_origin_px.x.into();
    let viewport_origin_y: f32 = ctx.viewport_origin_px.y.into();
    let panel_x = sx - panel_origin_x;
    let panel_y = sy - panel_origin_y;
    let viewport_x = sx - viewport_origin_x;
    let viewport_y = sy - viewport_origin_y;
    ArrangementLocalPoint {
        panel_x,
        panel_y,
        viewport_x,
        viewport_y,
        content_x: viewport_x + ctx.scroll_x_px,
        content_y: viewport_y + ctx.scroll_y_px,
    }
}

pub fn arrangement_x_to_beat(viewport_x: f32, ctx: &ArrangementCoordinateContext) -> f64 {
    ((viewport_x + ctx.scroll_x_px) / ctx.zoom_px_per_beat.max(0.0001)).max(0.0) as f64
}

pub fn arrangement_y_to_track_lane(viewport_y: f32, state: &TimelineState) -> Option<TrackId> {
    state.lane_y_to_track_id(viewport_y)
}

pub fn hit_test_arrangement(
    state: &TimelineState,
    screen: gpui::Point<gpui::Pixels>,
    ctx: &ArrangementCoordinateContext,
) -> ArrangementHitTestResult {
    let local = screen_to_arrangement_local(screen, ctx);
    let beat = arrangement_x_to_beat(local.viewport_x, ctx);

    if local.panel_y >= 0.0
        && local.panel_y < ctx.ruler_height_px
        && local.panel_x >= ctx.track_header_width_px
    {
        if let Some(marker) = marker_at_beat(state, beat, ctx) {
            return ArrangementHitTestResult {
                target: ArrangementHitTarget::Marker {
                    marker_id: marker.id.clone(),
                    timeline_beat: beat,
                },
                local,
                z_priority: 5,
            };
        }
        return ArrangementHitTestResult {
            target: ArrangementHitTarget::Ruler {
                timeline_beat: beat,
            },
            local,
            z_priority: 6,
        };
    }

    if local.viewport_y >= 0.0 && local.panel_x >= 0.0 && local.panel_x < ctx.track_header_width_px
    {
        if let Some(track_id) = arrangement_y_to_track_lane(local.viewport_y, state) {
            return ArrangementHitTestResult {
                target: ArrangementHitTarget::TrackHeader { track_id },
                local,
                z_priority: 7,
            };
        }
    }

    if local.viewport_x >= 0.0 && local.viewport_y >= 0.0 {
        if let Some(track_id) = arrangement_y_to_track_lane(local.viewport_y, state) {
            if let Some((track, clip)) = state
                .find_track(&track_id)
                .and_then(|track| clip_at_beat(track, beat).map(|clip| (track, clip)))
            {
                let local_beat = (beat - clip.start_beat as f64).max(0.0);
                let target = match clip.clip_type {
                    ClipType::Audio { .. } => ArrangementHitTarget::AudioClip {
                        track_id: track.id.clone(),
                        clip_id: clip.id.clone(),
                        timeline_beat: beat,
                        local_beat,
                    },
                    ClipType::Midi { .. } => ArrangementHitTarget::MidiClip {
                        track_id: track.id.clone(),
                        clip_id: clip.id.clone(),
                        timeline_beat: beat,
                        local_beat,
                    },
                    ClipType::Video { .. } => ArrangementHitTarget::VideoClip {
                        track_id: track.id.clone(),
                        clip_id: clip.id.clone(),
                        timeline_beat: beat,
                        local_beat,
                    },
                };
                return ArrangementHitTestResult {
                    target,
                    local,
                    z_priority: 3,
                };
            }

            if state.track_lane_mode(&track_id) == TrackLaneMode::Automation {
                if let Some(lane_id) = state.active_automation_lane_id(&track_id) {
                    return ArrangementHitTestResult {
                        target: ArrangementHitTarget::AutomationLane {
                            track_id,
                            lane_id,
                            timeline_beat: beat,
                        },
                        local,
                        z_priority: 4,
                    };
                }
            }

            return ArrangementHitTestResult {
                target: ArrangementHitTarget::TrackLane {
                    track_id,
                    timeline_beat: beat,
                },
                local,
                z_priority: 8,
            };
        }
    }

    ArrangementHitTestResult {
        target: ArrangementHitTarget::EmptyArrangement {
            timeline_beat: beat,
            track_id: arrangement_y_to_track_lane(local.viewport_y, state),
        },
        local,
        z_priority: 9,
    }
}

fn marker_at_beat<'a>(
    state: &'a TimelineState,
    beat: f64,
    ctx: &ArrangementCoordinateContext,
) -> Option<&'a TimelineMarkerState> {
    let beat_tolerance = 6.0 / ctx.zoom_px_per_beat.max(1.0) as f64;
    state
        .markers
        .iter()
        .filter(|marker| (marker.beat - beat).abs() <= beat_tolerance)
        .min_by(|a, b| {
            (a.beat - beat)
                .abs()
                .total_cmp(&(b.beat - beat).abs())
                .then_with(|| a.id.cmp(&b.id))
        })
}

fn clip_at_beat<'a>(track: &'a TrackState, beat: f64) -> Option<&'a ClipState> {
    track.clips.iter().rev().find(|clip| {
        // Not drawn in the lane, so not under the pointer there either.
        if track.is_hidden_take_clip(&clip.id) {
            return false;
        }
        let start = clip.start_beat as f64;
        let end = start + clip.duration_beats.max(0.0) as f64;
        beat >= start && beat <= end
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ArrangementCoordinateContext {
        ArrangementCoordinateContext {
            panel_origin_px: gpui::point(gpui::px(100.0), gpui::px(36.0)),
            viewport_origin_px: gpui::point(gpui::px(420.0), gpui::px(66.0)),
            scroll_x_px: 0.0,
            scroll_y_px: 0.0,
            zoom_px_per_beat: 100.0,
            ruler_height_px: RULER_HEIGHT,
            track_header_width_px: HEADER_WIDTH,
        }
    }

    fn state_with_audio_clip() -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        let track_id = state.create_audio_track();
        let clip = ClipState {
            id: "clip-a".to_string(),
            name: "Audio".to_string(),
            start_beat: 4.0,
            duration_beats: 2.0,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: "asset-a".to_string(),
                source_path: Some("C:/audio.wav".to_string()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        state
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .unwrap()
            .clips
            .push(clip);
        (state, track_id, "clip-a".to_string())
    }

    fn state_with_two_tracks() -> (TimelineState, String, String) {
        let mut state = TimelineState::default();
        let first = state.create_audio_track();
        let second = state.create_audio_track();
        (state, first, second)
    }

    #[test]
    fn right_click_on_audio_clip_returns_audio_clip_target() {
        let (state, track_id, clip_id) = state_with_audio_clip();
        let hit =
            hit_test_arrangement(&state, gpui::point(gpui::px(930.0), gpui::px(90.0)), &ctx());
        let ArrangementHitTarget::AudioClip {
            track_id: actual_track_id,
            clip_id: actual_clip_id,
            timeline_beat,
            local_beat,
        } = hit.target
        else {
            panic!("expected audio clip target");
        };
        assert_eq!(actual_track_id, track_id);
        assert_eq!(actual_clip_id, clip_id);
        assert!((timeline_beat - 5.1).abs() < 0.0001);
        assert!((local_beat - 1.1).abs() < 0.0001);
    }

    #[test]
    fn right_click_on_empty_track_lane_returns_track_lane_target() {
        let (state, track_id, _) = state_with_audio_clip();
        let hit = hit_test_arrangement(
            &state,
            gpui::point(gpui::px(1130.0), gpui::px(90.0)),
            &ctx(),
        );
        assert!(matches!(
            hit.target,
            ArrangementHitTarget::TrackLane {
                track_id: ref id,
                ..
            } if id == &track_id
        ));
    }

    #[test]
    fn right_click_on_track_header_returns_track_header_target() {
        let (state, track_id, _) = state_with_audio_clip();
        let hit =
            hit_test_arrangement(&state, gpui::point(gpui::px(180.0), gpui::px(90.0)), &ctx());
        assert_eq!(hit.target, ArrangementHitTarget::TrackHeader { track_id });
    }

    #[test]
    fn right_click_on_ruler_returns_ruler_target() {
        let (state, _, _) = state_with_audio_clip();
        let hit =
            hit_test_arrangement(&state, gpui::point(gpui::px(520.0), gpui::px(50.0)), &ctx());
        assert!(matches!(hit.target, ArrangementHitTarget::Ruler { .. }));
    }

    #[test]
    fn hit_test_respects_horizontal_scroll() {
        let (mut state, _, _) = state_with_audio_clip();
        state.viewport.scroll_x = 300.0;
        let mut ctx = ctx();
        ctx.scroll_x_px = 300.0;
        let hit = hit_test_arrangement(&state, gpui::point(gpui::px(630.0), gpui::px(90.0)), &ctx);
        assert!(matches!(hit.target, ArrangementHitTarget::AudioClip { .. }));
    }

    #[test]
    fn hit_test_respects_zoom() {
        let (mut state, _, _) = state_with_audio_clip();
        state.viewport.pixels_per_beat = 50.0;
        state.viewport.pixels_per_second = 100.0;
        let mut ctx = ctx();
        ctx.zoom_px_per_beat = 50.0;
        let hit = hit_test_arrangement(&state, gpui::point(gpui::px(675.0), gpui::px(90.0)), &ctx);
        assert!(matches!(hit.target, ArrangementHitTarget::AudioClip { .. }));
    }

    #[test]
    fn hit_test_respects_vertical_scroll() {
        let (mut state, _first, second) = state_with_two_tracks();
        state.viewport.scroll_y = TRACK_HEIGHT;
        let mut ctx = ctx();
        ctx.scroll_y_px = TRACK_HEIGHT;
        let hit = hit_test_arrangement(&state, gpui::point(gpui::px(1130.0), gpui::px(90.0)), &ctx);
        assert!(matches!(
            hit.target,
            ArrangementHitTarget::TrackLane {
                track_id: ref id,
                ..
            } if id == &second
        ));
    }
}
