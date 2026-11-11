//! Big Clock and Timecode — the playhead, large enough to read from the room.
//!
//! Two windows, one view. They differ only in which reading is the big one:
//!
//! ```txt
//! Big Clock   bars|beats large, wall time and timecode under it
//! Timecode    SMPTE large, bars|beats and wall time under it
//! ```
//!
//! Splitting them into separate components would mean two copies of the same
//! conversions and two chances for them to disagree about where the playhead
//! is, which is the one thing a clock may never do. Splitting them into
//! separate *windows* is right, though: an engineer watching timecode and a
//! player watching bars are two people, often on two monitors.
//!
//! Every conversion below is a free function over plain numbers, so the part
//! that can be wrong is the part that is tested. Drop-frame timecode in
//! particular is not a formatting choice — it is an arithmetic standard, and a
//! clock that gets it wrong is worse than one that does not offer it.

use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled,
    Window, WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{fb_segment, fb_segmented_track, FbSegment};
use crate::components::timeline::Timeline;
use crate::components::title_bar::external_window_titlebar;
use crate::theme::{radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

const WINDOW_WIDTH: f32 = 380.0;
const WINDOW_HEIGHT: f32 = 220.0;

/// How often the readout is refreshed.
///
/// 30 Hz, the rate the rest of Studio meters at. A clock updated faster than
/// the display refreshes is work nobody can see; one updated slower reads as
/// stuttering against the playhead beside it.
const REFRESH: Duration = Duration::from_millis(33);

/// Which reading this window leads with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockKind {
    BigClock,
    Timecode,
}

impl ClockKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::BigClock => "Big Clock",
            Self::Timecode => "Timecode",
        }
    }
}

/// Timecode rates a project is actually delivered at.
///
/// `Df2997` is the odd one and the reason this is an enum rather than a float:
/// 29.97 counts frames at 30 and drops labels to keep the count near wall
/// clock, so it is a different arithmetic and not a different number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimecodeRate {
    Fps24,
    Fps25,
    Df2997,
    Fps30,
}

/// The rates offered, in the order a picker should show them.
pub const TIMECODE_RATES: [TimecodeRate; 4] = [
    TimecodeRate::Fps24,
    TimecodeRate::Fps25,
    TimecodeRate::Df2997,
    TimecodeRate::Fps30,
];

impl TimecodeRate {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fps24 => "24",
            Self::Fps25 => "25",
            Self::Df2997 => "29.97 DF",
            Self::Fps30 => "30",
        }
    }

    /// Frames per second of the **count**, which for drop-frame is 30 — the
    /// dropping happens in the labels, never in the counting.
    pub fn nominal_fps(self) -> u32 {
        match self {
            Self::Fps24 => 24,
            Self::Fps25 => 25,
            Self::Df2997 | Self::Fps30 => 30,
        }
    }

    pub fn is_drop_frame(self) -> bool {
        matches!(self, Self::Df2997)
    }
}

/// Bar, beat and tick for a position in quarter-note beats.
///
/// All three are 1-based except the tick, because that is how a musician counts
/// and how every other readout in Studio is written. `ts_den` matters: in 6/8 a
/// bar is six eighth-notes, which is three quarter-note beats, so a bar is not
/// `ts_num` quarter-notes except by coincidence at `/4`.
pub fn bars_beats(position_beats: f64, ts_num: u32, ts_den: u32) -> (u32, u32, u32) {
    let ts_num = ts_num.max(1);
    let ts_den = ts_den.max(1);
    // One beat of the meter, in quarter notes.
    let beat_in_quarters = 4.0 / ts_den as f64;
    let bar_in_quarters = beat_in_quarters * ts_num as f64;

    let position = position_beats.max(0.0);
    let bar = (position / bar_in_quarters).floor();
    let into_bar = position - bar * bar_in_quarters;
    let beat = (into_bar / beat_in_quarters).floor();
    let into_beat = into_bar - beat * beat_in_quarters;
    // 960 ticks to a beat of the meter, the resolution the piano roll uses.
    let tick = ((into_beat / beat_in_quarters) * 960.0).floor();

    (
        bar as u32 + 1,
        (beat as u32).min(ts_num - 1) + 1,
        tick.clamp(0.0, 959.0) as u32,
    )
}

/// `bar|beat|tick`, zero-padded so the readout does not jump width as it counts.
pub fn bars_beats_text(position_beats: f64, ts_num: u32, ts_den: u32) -> String {
    let (bar, beat, tick) = bars_beats(position_beats, ts_num, ts_den);
    format!("{bar:>4}|{beat}|{tick:03}")
}

/// Wall time as `H:MM:SS.mmm`.
pub fn clock_text(position_seconds: f64) -> String {
    let total = position_seconds.max(0.0);
    let hours = (total / 3600.0).floor() as u64;
    let minutes = ((total % 3600.0) / 60.0).floor() as u64;
    let seconds = (total % 60.0).floor() as u64;
    let millis = ((total - total.floor()) * 1000.0).floor() as u64;
    format!("{hours}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// SMPTE timecode as `HH:MM:SS:FF`.
///
/// Non-drop rates are a plain division. Drop-frame is the standard 29.97
/// correction: the count runs at 30 fps against a clock that is 0.1% slow, so
/// two frame *labels* (`:00` and `:01`) are skipped at the top of every minute
/// except every tenth. That keeps the label within a couple of frames of wall
/// time over an hour instead of drifting 3.6 seconds. The frames are never
/// dropped — only their names — which is why this operates on a frame number
/// and not on the seconds directly.
pub fn timecode_text(position_seconds: f64, rate: TimecodeRate) -> String {
    let fps = rate.nominal_fps() as f64;
    let seconds = position_seconds.max(0.0);

    if !rate.is_drop_frame() {
        let total_frames = (seconds * fps).floor() as u64;
        let fps = rate.nominal_fps() as u64;
        let frames = total_frames % fps;
        let total_seconds = total_frames / fps;
        return format!(
            "{:02}:{:02}:{:02}:{:02}",
            total_seconds / 3600,
            (total_seconds / 60) % 60,
            total_seconds % 60,
            frames
        );
    }

    // The count is 30 fps against a 29.97 clock, so a wall second holds
    // 30 * (1000/1001) counted frames.
    let counted = (seconds * 30.0 * 1000.0 / 1001.0).floor() as i64;
    let drop_per_minute = 2i64;
    let frames_per_10_minutes = 17_982i64; // 10 minutes of labels, two dropped in nine of them
    let frames_per_minute = 1_798i64;

    let ten_minute_blocks = counted / frames_per_10_minutes;
    let remainder = counted % frames_per_10_minutes;
    // The first minute of each ten drops nothing, so it is a frame longer.
    let dropped = drop_per_minute
        * (9 * ten_minute_blocks
            + if remainder >= drop_per_minute {
                (remainder - drop_per_minute) / frames_per_minute
            } else {
                0
            });
    let labelled = counted + dropped;

    format!(
        "{:02}:{:02}:{:02}:{:02}",
        labelled / 108_000,
        (labelled / 1_800) % 60,
        (labelled / 30) % 60,
        labelled % 30
    )
}

/// Everything the readout needs, resolved once per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ClockReading {
    position_beats: f64,
    position_seconds: f64,
    bpm: f64,
    ts_num: u32,
    ts_den: u32,
    playing: bool,
    recording: bool,
}

pub struct ClockWindow {
    kind: ClockKind,
    timeline: gpui::Entity<Timeline>,
    rate: TimecodeRate,
    on_close: Arc<dyn Fn(ClockKind, &mut App) + Send + Sync>,
}

impl ClockWindow {
    fn new(
        kind: ClockKind,
        timeline: gpui::Entity<Timeline>,
        on_close: Arc<dyn Fn(ClockKind, &mut App) + Send + Sync>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The playhead moves whether or not the project view is being edited,
        // so the window drives its own repaint rather than waiting to be told.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH).await;
            if this.update(cx, |_this, cx| cx.notify()).is_err() {
                break;
            }
        })
        .detach();

        Self {
            kind,
            timeline,
            rate: TimecodeRate::Fps25,
            on_close,
        }
    }

    fn reading(&self, cx: &App) -> ClockReading {
        let timeline = self.timeline.read(cx);
        let state = &timeline.state;
        let position_beats = state.transport.playhead_beats.max(0.0) as f64;
        let signature = state.time_signature_at_playhead();
        // Seconds from the tempo map rather than from `beats / bpm`: a project
        // with a tempo change has no single bpm, and a clock that assumes one
        // is wrong from the first ramp onwards.
        let position_seconds = state.seconds_at_beat(position_beats);
        ClockReading {
            position_beats,
            position_seconds,
            bpm: state.effective_bpm_at_playhead(),
            ts_num: u32::from(signature.numerator.max(1)),
            ts_den: u32::from(signature.denominator.max(1)),
            playing: state.transport.playing,
            recording: state.transport.recording,
        }
    }

    fn primary(&self, reading: &ClockReading) -> String {
        match self.kind {
            ClockKind::BigClock => {
                bars_beats_text(reading.position_beats, reading.ts_num, reading.ts_den)
            }
            ClockKind::Timecode => timecode_text(reading.position_seconds, self.rate),
        }
    }

    /// The two readings that are not the headline, in a fixed order so the eye
    /// learns where each one lives.
    fn secondary(&self, reading: &ClockReading) -> [(&'static str, String); 2] {
        match self.kind {
            ClockKind::BigClock => [
                ("Time", clock_text(reading.position_seconds)),
                ("TC", timecode_text(reading.position_seconds, self.rate)),
            ],
            ClockKind::Timecode => [
                (
                    "Bars",
                    bars_beats_text(reading.position_beats, reading.ts_num, reading.ts_den),
                ),
                ("Time", clock_text(reading.position_seconds)),
            ],
        }
    }
}

impl Render for ClockWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reading = self.reading(cx);
        let kind = self.kind;
        let on_close = self.on_close.clone();
        // Recording outranks playing: the two are not exclusive on screen, and
        // the one that matters when both are true is the one that is writing.
        let (tone, state_label) = if reading.recording {
            (Colors::status_error(), "Recording")
        } else if reading.playing {
            (Colors::status_success(), "Playing")
        } else {
            (Colors::text_faint(), "Stopped")
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(crate::theme::ui_font())
            .child(external_window_titlebar(
                kind.title(),
                "clock-window-close",
                move |window, cx| {
                    on_close(kind, cx);
                    window.remove_window();
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .items_center()
                    .justify_center()
                    .gap(px(space::BASE))
                    .px(px(space::SECTION))
                    .child(
                        // Tabular figures: a clock whose digits change width
                        // jitters sideways as it counts, which is unreadable at
                        // a glance and is the whole point of the window.
                        div()
                            .text_size(px(46.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(if reading.recording {
                                Colors::status_error()
                            } else {
                                Colors::text_primary()
                            })
                            .child(self.primary(&reading)),
                    )
                    .child(div().flex().flex_row().gap(px(space::SECTION)).children(
                        self.secondary(&reading).map(|(label, value)| {
                            div()
                                .flex()
                                .flex_row()
                                .items_baseline()
                                .gap(px(space::SNUG))
                                .child(
                                    div()
                                        .text_size(px(typography::UI_XS))
                                        .text_color(Colors::text_muted())
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .text_size(px(typography::UI_SM))
                                        .text_color(Colors::text_secondary())
                                        .child(value),
                                )
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(space::BASE))
                    .flex_none()
                    .px(px(space::SECTION))
                    .py(px(space::BASE))
                    .border_t(px(1.0))
                    .border_color(Colors::border_subtle())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(space::SNUG))
                            .child(
                                div()
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded(px(radius::PILL))
                                    .bg(tone),
                            )
                            .child(
                                div()
                                    .text_size(px(typography::UI_XS))
                                    .text_color(Colors::text_muted())
                                    .child(format!(
                                        "{state_label} · {:.2} BPM · {}/{}",
                                        reading.bpm, reading.ts_num, reading.ts_den
                                    )),
                            ),
                    )
                    // The rate belongs to timecode, so only the window that
                    // leads with timecode offers it. Putting it on both would
                    // imply the Big Clock's bars depend on it.
                    .when(kind == ClockKind::Timecode, |footer| {
                        let mut track = fb_segmented_track();
                        let count = TIMECODE_RATES.len();
                        for (index, rate) in TIMECODE_RATES.into_iter().enumerate() {
                            let position = if index == 0 {
                                FbSegment::First
                            } else if index + 1 == count {
                                FbSegment::Last
                            } else {
                                FbSegment::Middle
                            };
                            track = track.child(fb_segment(
                                gpui::ElementId::Name(format!("clock-rate-{index}").into()),
                                rate.label(),
                                rate == self.rate,
                                position,
                                cx.listener(move |this, _event, _window, cx| {
                                    this.rate = rate;
                                    cx.notify();
                                }),
                            ));
                        }
                        footer.child(track)
                    }),
            )
    }
}

/// Open a clock window.
pub fn open_clock_window(
    kind: ClockKind,
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    timeline: gpui::Entity<Timeline>,
    on_close: Arc<dyn Fn(ClockKind, &mut App) + Send + Sync>,
    cx: &mut App,
) -> Result<WindowHandle<ClockWindow>, String> {
    let mut options = crate::platform_chrome::external_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(centered_window_bounds(
        owner_bounds,
        size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)),
        cx,
    )));
    options.kind = WindowKind::Normal;
    options.is_resizable = true;
    apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| ClockWindow::new(kind, timeline, on_close, cx))
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{bars_beats, bars_beats_text, clock_text, timecode_text, TimecodeRate};

    /// Bar one, beat one is where a project starts, and a musician counts from
    /// one. A clock that opens on `0|0|000` is off by a bar for the whole
    /// session.
    #[test]
    fn the_first_beat_of_a_project_is_bar_one_beat_one() {
        assert_eq!(bars_beats(0.0, 4, 4), (1, 1, 0));
        assert_eq!(bars_beats(1.0, 4, 4), (1, 2, 0));
        assert_eq!(bars_beats(4.0, 4, 4), (2, 1, 0));
    }

    /// A bar is `ts_num` beats *of the meter*, and a beat of the meter is only
    /// a quarter note at `/4`. In 6/8 a bar is three quarter notes, so reading
    /// the numerator as quarter notes puts the downbeat in the wrong place from
    /// bar two onwards.
    #[test]
    fn the_denominator_decides_how_long_a_bar_is() {
        // 6/8: six eighths = three quarter notes to the bar.
        assert_eq!(bars_beats(0.0, 6, 8), (1, 1, 0));
        assert_eq!(bars_beats(0.5, 6, 8), (1, 2, 0));
        assert_eq!(bars_beats(3.0, 6, 8), (2, 1, 0));
        // 3/4: three quarter notes.
        assert_eq!(bars_beats(3.0, 3, 4), (2, 1, 0));
    }

    /// Before the start is still bar one: the playhead can be dragged to a
    /// negative position mid-gesture and a clock must not show a negative bar.
    #[test]
    fn a_position_before_the_start_reads_as_the_first_beat() {
        assert_eq!(bars_beats(-4.0, 4, 4), (1, 1, 0));
    }

    /// The readout is fixed width or it jitters sideways as it counts, which is
    /// unreadable at the distance this window exists to be read from.
    #[test]
    fn the_bars_readout_keeps_its_width_as_it_counts() {
        let early = bars_beats_text(0.0, 4, 4);
        let late = bars_beats_text(4.0 * 998.0, 4, 4);
        assert_eq!(early.len(), late.len());
        assert!(early.ends_with("1|1|000"));
    }

    #[test]
    fn wall_time_counts_hours_minutes_seconds_and_milliseconds() {
        assert_eq!(clock_text(0.0), "0:00:00.000");
        assert_eq!(clock_text(61.5), "0:01:01.500");
        assert_eq!(clock_text(3661.25), "1:01:01.250");
        assert_eq!(clock_text(-5.0), "0:00:00.000", "before the start is zero");
    }

    /// Non-drop rates are a plain division, and the frame number must roll over
    /// exactly at the rate rather than one frame late.
    #[test]
    fn non_drop_timecode_rolls_over_at_its_own_rate() {
        assert_eq!(timecode_text(0.0, TimecodeRate::Fps25), "00:00:00:00");
        assert_eq!(timecode_text(0.04, TimecodeRate::Fps25), "00:00:00:01");
        assert_eq!(timecode_text(1.0, TimecodeRate::Fps25), "00:00:01:00");
        assert_eq!(timecode_text(1.0, TimecodeRate::Fps24), "00:00:01:00");
        assert_eq!(timecode_text(1.0, TimecodeRate::Fps30), "00:00:01:00");
        assert_eq!(timecode_text(3661.0, TimecodeRate::Fps25), "01:01:01:00");
    }

    /// Drop-frame is an arithmetic standard, not a formatting preference.
    ///
    /// A 29.97 signal delivers 30000/1001 frames a second, so a wall minute is
    /// 1798 frames rather than 1800 and the labels fall behind. The correction
    /// skips the labels `:00` and `:01` at the top of every minute except every
    /// tenth, which is what pulls them back level — at exactly ten minutes the
    /// label reads 10:00:00 while only 17982 frames have gone by.
    #[test]
    fn drop_frame_labels_catch_wall_time_up_over_ten_minutes() {
        assert_eq!(timecode_text(0.0, TimecodeRate::Df2997), "00:00:00:00");

        // Nothing is dropped inside the first minute — the first skip is at the
        // *start* of minute one — so a wall minute is still two frames short.
        assert_eq!(timecode_text(60.0, TimecodeRate::Df2997), "00:00:59:28");

        // Nine minutes of skipped labels later, they are level again. This is
        // the number the whole scheme exists to produce.
        assert_eq!(timecode_text(600.0, TimecodeRate::Df2997), "00:10:00:00");

        // And that label is ahead of the raw count it was made from: 18000
        // labelled where 17982 elapsed. Counting at a true 30 fps over the same
        // wall time gives the same label from a different number of frames,
        // which is exactly the distinction drop-frame encodes.
        assert_eq!(timecode_text(600.0, TimecodeRate::Fps30), "00:10:00:00");
    }

    /// Where drop-frame and a true 30 differ is over the same *frame count*,
    /// not the same wall time: a 29.97 project running for an hour of wall
    /// clock has delivered 107 892 frames, and only drop-frame labels that as
    /// an hour.
    #[test]
    fn an_hour_of_wall_clock_reads_as_an_hour_only_in_drop_frame() {
        assert_eq!(timecode_text(3600.0, TimecodeRate::Df2997), "01:00:00:00");
        // The same wall hour counted at 29.97 and labelled without dropping is
        // the 3.6 seconds of drift the correction removes.
        let counted = (3600.0f64 * 30.0 * 1000.0 / 1001.0).floor();
        assert!(
            (107_892.0 - counted).abs() < 1.0,
            "a wall hour is 107892 frames at 29.97, got {counted}"
        );
    }

    /// Every rate produces the same shape, because the window's layout is
    /// fixed and a shorter string would move the digits under the eye.
    #[test]
    fn every_rate_produces_the_same_shape() {
        for rate in super::TIMECODE_RATES {
            let text = timecode_text(1234.5, rate);
            assert_eq!(text.len(), 11, "{rate:?} produced {text}");
            assert_eq!(text.matches(':').count(), 3, "{rate:?} produced {text}");
        }
    }
}
