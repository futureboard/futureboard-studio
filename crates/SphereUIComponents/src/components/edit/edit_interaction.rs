//! Shared edit-tool semantics for arrangement and piano-roll surfaces.

use gpui::Modifiers;

use crate::components::timeline::timeline_state::{TimelineTool, TrackType};

/// Small drag threshold (px) before a press becomes a drag gesture.
pub const EDIT_DRAG_THRESHOLD_PX: f32 = 4.0;

/// What a left press on arrangement lane space that no clip owns starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanePressIntent {
    /// A free rubber-band selection. `additive` keeps what is selected.
    /// `create_clip_on_click` arms a default-length clip that is created only
    /// when the press is released without becoming a drag — a drag is still a
    /// marquee.
    Marquee {
        additive: bool,
        create_clip_on_click: bool,
    },
    /// The Pen tool's create gesture (a sized MIDI clip, or a copy of the
    /// selected audio clip). The track type decides what, if anything, it
    /// creates.
    Pen,
    /// Every other tool: the press only chooses the track.
    SelectTrack,
    /// Nothing to do (a non-Pointer press below the last track).
    Ignore,
}

/// Whether `modifiers` make a marquee additive.
///
/// Cmd on macOS, Ctrl elsewhere. On macOS GPUI turns a Ctrl + left press into
/// a right press (the system's secondary click), so a Ctrl-drag there opens a
/// context menu instead of ever reaching a marquee.
pub fn marquee_additive(modifiers: &Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}

/// Classify a left press on arrangement lane space no clip owns.
///
/// `lane` is the track type of the lane pressed, or `None` for the empty area
/// below the last track. With the Pointer every lane starts a marquee — MIDI
/// and Instrument lanes included — and a double-click on an empty MIDI or
/// Instrument lane additionally arms a default-length clip for a release
/// without drag. The Pen keeps its drag-to-size create gesture.
pub fn lane_press_intent(
    tool: TimelineTool,
    lane: Option<TrackType>,
    click_count: usize,
    modifiers: &Modifiers,
) -> LanePressIntent {
    let Some(lane) = lane else {
        return if tool == TimelineTool::Pointer {
            LanePressIntent::Marquee {
                additive: marquee_additive(modifiers),
                create_clip_on_click: false,
            }
        } else {
            LanePressIntent::Ignore
        };
    };
    match tool {
        TimelineTool::Pointer => {
            let additive = marquee_additive(modifiers);
            LanePressIntent::Marquee {
                additive,
                create_clip_on_click: click_count >= 2
                    && !additive
                    && matches!(lane, TrackType::Midi | TrackType::Instrument),
            }
        }
        TimelineTool::Pen => LanePressIntent::Pen,
        TimelineTool::Cut
        | TimelineTool::Glue
        | TimelineTool::Mute
        | TimelineTool::Time
        | TimelineTool::Automation => LanePressIntent::SelectTrack,
    }
}

/// Whether a marquee press at `press` has become a drag at `current`, both in
/// window pixels. Measured on the raw pointer, never on snapped or row-quantized
/// positions, so a small move in any direction starts the rectangle.
pub fn marquee_drag_started(press: (f32, f32), current: (f32, f32)) -> bool {
    let dx = current.0 - press.0;
    let dy = current.1 - press.1;
    dx.hypot(dy) >= EDIT_DRAG_THRESHOLD_PX
}

/// Normalize a 1-D range so `start <= end`.
#[inline]
pub fn normalize_range(start: f32, end: f32) -> (f32, f32) {
    if start <= end {
        (start, end)
    } else {
        (end, start)
    }
}

/// Axis-aligned rectangle intersection (left, top, right, bottom).
#[inline]
pub fn rects_intersect(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(platform: bool, control: bool, shift: bool) -> Modifiers {
        Modifiers {
            platform,
            control,
            shift,
            ..Modifiers::default()
        }
    }

    fn marquee(additive: bool, create_clip_on_click: bool) -> LanePressIntent {
        LanePressIntent::Marquee {
            additive,
            create_clip_on_click,
        }
    }

    #[test]
    fn lane_press_intent_table() {
        let plain = Modifiers::default();
        let additive = if cfg!(target_os = "macos") {
            mods(true, false, false)
        } else {
            mods(false, true, false)
        };
        let lanes = [
            TrackType::Audio,
            TrackType::Midi,
            TrackType::Instrument,
            TrackType::Group,
            TrackType::Video,
        ];

        // Pointer: a single press starts a marquee on every lane, MIDI and
        // Instrument included.
        for lane in lanes {
            assert_eq!(
                lane_press_intent(TimelineTool::Pointer, Some(lane), 1, &plain),
                marquee(false, false),
                "{lane:?}"
            );
            assert_eq!(
                lane_press_intent(TimelineTool::Pointer, Some(lane), 1, &additive),
                marquee(true, false),
                "{lane:?}"
            );
        }

        // A double-click on an empty MIDI / Instrument lane arms a clip for a
        // release without drag; it is still a marquee if it drags.
        for lane in [TrackType::Midi, TrackType::Instrument] {
            assert_eq!(
                lane_press_intent(TimelineTool::Pointer, Some(lane), 2, &plain),
                marquee(false, true)
            );
            assert_eq!(
                lane_press_intent(TimelineTool::Pointer, Some(lane), 2, &additive),
                marquee(true, false),
                "an additive double-click only extends the selection"
            );
        }
        assert_eq!(
            lane_press_intent(TimelineTool::Pointer, Some(TrackType::Audio), 2, &plain),
            marquee(false, false)
        );

        // Shift is not the additive modifier.
        assert_eq!(
            lane_press_intent(
                TimelineTool::Pointer,
                Some(TrackType::Audio),
                1,
                &mods(false, false, true)
            ),
            marquee(false, false)
        );

        // Pen keeps drawing.
        assert_eq!(
            lane_press_intent(TimelineTool::Pen, Some(TrackType::Midi), 1, &plain),
            LanePressIntent::Pen
        );
        assert_eq!(
            lane_press_intent(TimelineTool::Pen, Some(TrackType::Audio), 1, &plain),
            LanePressIntent::Pen
        );

        // Every other tool only chooses the track.
        for tool in [
            TimelineTool::Cut,
            TimelineTool::Glue,
            TimelineTool::Mute,
            TimelineTool::Time,
            TimelineTool::Automation,
        ] {
            assert_eq!(
                lane_press_intent(tool, Some(TrackType::Audio), 1, &plain),
                LanePressIntent::SelectTrack,
                "{tool:?}"
            );
            assert_eq!(
                lane_press_intent(tool, None, 1, &plain),
                LanePressIntent::Ignore,
                "{tool:?} below the last track"
            );
        }

        // Below the last track the Pointer still starts a marquee.
        assert_eq!(
            lane_press_intent(TimelineTool::Pointer, None, 1, &plain),
            marquee(false, false)
        );
        assert_eq!(
            lane_press_intent(TimelineTool::Pointer, None, 2, &additive),
            marquee(true, false)
        );
    }

    /// Ctrl + left is a right press on macOS, so only Cmd can be the additive
    /// marquee modifier there; elsewhere it is Ctrl.
    #[test]
    fn the_additive_marquee_modifier_follows_the_platform() {
        let cmd = mods(true, false, false);
        let ctrl = mods(false, true, false);
        if cfg!(target_os = "macos") {
            assert!(marquee_additive(&cmd));
            assert!(!marquee_additive(&ctrl));
        } else {
            assert!(marquee_additive(&ctrl));
            assert!(!marquee_additive(&cmd));
        }
        assert!(!marquee_additive(&Modifiers::default()));
    }

    #[test]
    fn a_marquee_starts_on_raw_pointer_travel_in_any_direction() {
        let press = (100.0, 200.0);
        // Vertically inside one track: the old row-quantized test never saw it.
        assert!(marquee_drag_started(press, (100.0, 205.0)));
        assert!(!marquee_drag_started(press, (100.0, 203.0)));
        assert!(marquee_drag_started(press, (96.0, 200.0)));
        assert!(!marquee_drag_started(press, (102.0, 202.0)));
        assert!(marquee_drag_started(press, (103.0, 203.0)));
    }
}
