//! The docked mixer's meters, painted apart from the strips they sit in.
//!
//! A meter moves at the display refresh for as long as anything is audible;
//! everything else on a strip — racks, sends, pan, fader, plate — changes when
//! the user does something. Drawing the meter inside the strip tied the two
//! together: every meter tick rebuilt every visible strip's element tree,
//! plug-in chips and send sliders included, so a large session stuttered in
//! playback for no reason but a few pixels of green.
//!
//! So the strips leave an empty [`meter_slot`] where each meter goes and record
//! its window bounds as they are laid out; [`MixerMeterOverlay`] paints every
//! recorded slot with the live levels from one canvas.
//!
//! **Where the overlay lives matters.** GPUI marks every ancestor of a notified
//! view dirty, and a cached view that re-renders refreshes everything inside
//! it. An overlay inside the mixer — or anywhere under the cached bottom panel
//! — would rebuild the whole panel on each tick all the same. It is therefore
//! rendered by the studio root, beside the bottom panel rather than in it: a
//! meter tick re-renders the root (which the playhead already does every
//! frame) and the overlay, and the bottom panel is reused as it was drawn.
//! Its slots stay valid for exactly as long as that reuse does; any rebuild of
//! the strips records them again.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    Bounds, ContentMask, Context, Entity, IntoElement, ParentElement, Pixels, Render, Styled,
    Window, canvas, div, px,
};

use crate::components::mixer_panel::{VstiOutputMeterState, vsti_output_meter_key};
use crate::components::timeline::timeline::Timeline;
use crate::components::timeline::vu_meter::{STEREO_METER_WIDTH, paint_stereo_meter};
use crate::layout::StudioLayout;

/// Where the meter of a VSTi output channel reads from when its backing track
/// has no level yet: the per-channel plug-in output meters, left and right.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VstiMeterFallback {
    pub parent_track_id: String,
    pub insert_id: String,
    pub channel_l: u8,
    pub channel_r: u8,
}

/// What a slot meters.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MeterSource {
    Track {
        id: String,
        /// Index of `id` in the timeline when the strips were built; checked
        /// against the id before use, so a reordered track list falls back to
        /// a search instead of painting another channel's level.
        index: usize,
        vsti: Option<VstiMeterFallback>,
    },
    Master,
    Monitor,
}

impl MeterSource {
    fn is_pinned(&self) -> bool {
        matches!(self, Self::Master | Self::Monitor)
    }

    /// A slot per source: a rebuilt strip replaces its own entry.
    fn key(&self) -> String {
        match self {
            Self::Track { id, .. } => id.clone(),
            Self::Master => "\u{0}master".to_string(),
            Self::Monitor => "\u{0}monitor".to_string(),
        }
    }
}

/// What a recorded slot shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SlotKind {
    /// The stereo bar.
    Meter,
    /// The peak-hold readout under the fader bay.
    Peak,
}

/// The meters the docked mixer laid out in its last build, and the regions
/// they may paint in.
#[derive(Default)]
pub struct MeterLayout {
    slots: HashMap<(SlotKind, String), (MeterSource, Bounds<Pixels>)>,
    /// The channel strips' scroll viewport. A strip scrolled half out of it
    /// must not paint its meter over the Master beside it.
    strip_clip: Option<Bounds<Pixels>>,
    /// The mixer body: nothing paints outside it, pinned strips included.
    body_clip: Option<Bounds<Pixels>>,
}

impl MeterLayout {
    /// The channel strips are rebuilding: forget their slots and regions. The
    /// pinned strips keep theirs — they are a separate view that rebuilds on
    /// its own schedule (see [`Self::begin_pinned_build`]).
    pub(crate) fn begin_strips_build(&mut self) {
        self.slots.retain(|_, (source, _)| source.is_pinned());
        self.strip_clip = None;
        self.body_clip = None;
    }

    pub(crate) fn begin_pinned_build(&mut self) {
        self.slots.retain(|_, (source, _)| !source.is_pinned());
    }

    /// The mixer is not on screen: paint nothing until it is laid out again.
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.strip_clip = None;
        self.body_clip = None;
    }
}

pub type SharedMeterLayout = Rc<RefCell<MeterLayout>>;

/// The place a strip's meter goes: the meter's width, the bay's height, and
/// nothing painted. Its bounds are recorded for the overlay as it is laid out.
pub(crate) fn meter_slot(layout: &SharedMeterLayout, source: MeterSource) -> impl IntoElement {
    let layout = layout.clone();
    div().flex_none().w(px(STEREO_METER_WIDTH)).h_full().child(
        canvas(
            move |bounds, _window, _cx| {
                layout
                    .borrow_mut()
                    .slots
                    .insert((SlotKind::Meter, source.key()), (source.clone(), bounds));
            },
            |_bounds, _state, _window, _cx| {},
        )
        .size_full(),
    )
}

/// Where a strip's peak readout prints its value: the readout's box, empty.
/// The box itself (fill, radius) is the strip's; the layer paints the number,
/// and a clip.
pub(crate) fn peak_slot(layout: &SharedMeterLayout, source: MeterSource) -> impl IntoElement {
    let layout = layout.clone();
    canvas(
        move |bounds, _window, _cx| {
            layout
                .borrow_mut()
                .slots
                .insert((SlotKind::Peak, source.key()), (source.clone(), bounds));
        },
        |_bounds, _state, _window, _cx| {},
    )
    .size_full()
}

/// The peak readout's text: the held peak in dBFS, `-∞` for silence, `CLIP`
/// once the channel has reached full scale. Levels arrive clamped to full
/// scale, so the clip flag, not the number, is what says it went over.
pub(crate) fn peak_readout_text(hold_l: f32, hold_r: f32, clip: bool) -> String {
    if clip {
        return "CLIP".to_string();
    }
    let peak = hold_l.max(hold_r);
    if peak <= 1.0e-5 {
        "-\u{221e}".to_string()
    } else {
        format!("{:.1}", 20.0 * peak.log10())
    }
}

/// Size of the peak readout's figures.
pub(crate) const PEAK_TEXT_SIZE: f32 = 9.5;

/// Which clip region a [`clip_region`] records.
#[derive(Clone, Copy)]
pub(crate) enum ClipRegion {
    Strips,
    Body,
}

/// An invisible full-size child that records its parent's bounds as a clip
/// region. Put it in the region's container; it takes no input.
pub(crate) fn clip_region(layout: &SharedMeterLayout, region: ClipRegion) -> impl IntoElement {
    let layout = layout.clone();
    canvas(
        move |bounds, _window, _cx| {
            let mut layout = layout.borrow_mut();
            match region {
                ClipRegion::Strips => layout.strip_clip = Some(bounds),
                ClipRegion::Body => layout.body_clip = Some(bounds),
            }
        },
        |_bounds, _state, _window, _cx| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Paints the recorded meters with the current levels. Rendered by the
/// studio root over the whole window while the docked mixer is showing.
pub struct MixerMeterOverlay {
    owner: Entity<StudioLayout>,
    timeline: Entity<Timeline>,
    layout: SharedMeterLayout,
}

impl MixerMeterOverlay {
    pub(crate) fn new(
        owner: Entity<StudioLayout>,
        timeline: Entity<Timeline>,
        layout: SharedMeterLayout,
    ) -> Self {
        Self {
            owner,
            timeline,
            layout,
        }
    }
}

/// What one meter shows.
#[derive(Clone, Copy, Default)]
struct MeterLevels {
    level_l: f32,
    level_r: f32,
    hold_l: f32,
    hold_r: f32,
    clip: bool,
}

impl Render for MixerMeterOverlay {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("MixerMeterOverlay");
        // Resolve the levels here, where the entities can be read; the canvas
        // only paints.
        let (strip_clip, body_clip, meters) = {
            let layout = self.layout.borrow();
            let state = &self.timeline.read(cx).state;
            let vsti_meters = self
                .owner
                .read(cx)
                .docked_mixer_panel_state(cx)
                .vsti_output_meters;
            let meters: Vec<(SlotKind, bool, Bounds<Pixels>, MeterLevels)> = layout
                .slots
                .iter()
                .map(|((kind, _), (source, bounds))| {
                    let levels = match source {
                        MeterSource::Track { id, index, vsti } => {
                            let track = state
                                .tracks
                                .get(*index)
                                .filter(|track| &track.id == id)
                                .or_else(|| state.tracks.iter().find(|track| &track.id == id));
                            let mut levels = track
                                .map(|track| MeterLevels {
                                    level_l: track.meter_level_l,
                                    level_r: track.meter_level_r,
                                    hold_l: track.meter_peak_hold_l,
                                    hold_r: track.meter_peak_hold_r,
                                    clip: track.meter_clip,
                                })
                                .unwrap_or_default();
                            if let Some(vsti) = vsti.as_ref() {
                                apply_vsti_fallback(&mut levels, vsti, vsti_meters);
                            }
                            levels
                        }
                        MeterSource::Master => MeterLevels {
                            level_l: state.master.meter_level_l,
                            level_r: state.master.meter_level_r,
                            hold_l: state.master.meter_peak_hold_l,
                            hold_r: state.master.meter_peak_hold_r,
                            clip: state.master.meter_clip,
                        },
                        MeterSource::Monitor => MeterLevels {
                            level_l: state.monitor.meter_level_l,
                            level_r: state.monitor.meter_level_r,
                            hold_l: state.monitor.meter_peak_hold_l,
                            hold_r: state.monitor.meter_peak_hold_r,
                            clip: state.monitor.meter_clip,
                        },
                    };
                    (*kind, source.is_pinned(), *bounds, levels)
                })
                .collect();
            (layout.strip_clip, layout.body_clip, meters)
        };
        canvas(
            |_bounds, _window, _cx| (),
            move |_bounds, _state, window, cx| {
                let Some(body) = body_clip else {
                    return;
                };
                for (kind, pinned, slot, m) in &meters {
                    let clip = if *pinned {
                        body
                    } else {
                        match strip_clip {
                            Some(strips) => strips.intersect(&body),
                            None => continue,
                        }
                    };
                    window.with_content_mask(
                        Some(ContentMask { bounds: clip }),
                        |window| match kind {
                            SlotKind::Meter => paint_stereo_meter(
                                *slot, m.level_l, m.level_r, m.hold_l, m.hold_r, m.clip, window,
                            ),
                            SlotKind::Peak => paint_peak_readout(*slot, m, window, cx),
                        },
                    );
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

/// The peak readout's number, centred in its box; on a clip, the box turns
/// red under it. Two channels — fill and word — so a clip is not colour alone.
fn paint_peak_readout(
    bounds: Bounds<Pixels>,
    m: &MeterLevels,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    use crate::theme::Colors;
    let text = peak_readout_text(m.hold_l, m.hold_r, m.clip);
    let color = if m.clip {
        window.paint_quad(
            gpui::fill(bounds, Colors::status_error())
                .corner_radii(px(crate::theme::radius::MICRO)),
        );
        Colors::on_color(Colors::status_error())
    } else {
        Colors::text_secondary()
    };
    let run = gpui::TextRun {
        len: text.len(),
        font: crate::theme::ui_font_weight(gpui::FontWeight::SEMIBOLD),
        color: color.into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .shape_line(text.into(), px(PEAK_TEXT_SIZE), &[run], None);
    let line_height = bounds.size.height;
    let origin = gpui::point(
        bounds.origin.x + (bounds.size.width - line.width).max(px(0.0)) / 2.0,
        bounds.origin.y,
    );
    let _ = line.paint(origin, line_height, gpui::TextAlign::Left, None, window, cx);
}

/// A VSTi output channel whose backing track has not been metered yet reads
/// the plug-in's own per-channel output meters — the same rule the strip used
/// when it drew its meter itself.
fn apply_vsti_fallback(
    levels: &mut MeterLevels,
    vsti: &VstiMeterFallback,
    meters: &HashMap<String, VstiOutputMeterState>,
) {
    if levels.level_l > 0.0 || levels.level_r > 0.0 {
        return;
    }
    let key = |channel| vsti_output_meter_key(&vsti.parent_track_id, &vsti.insert_id, channel);
    if let Some(meter) = meters.get(&key(vsti.channel_l)) {
        levels.level_l = meter.level;
        levels.hold_l = meter.peak_hold;
        levels.clip |= meter.clip;
    }
    if let Some(meter) = meters.get(&key(vsti.channel_r)) {
        levels.level_r = meter.level;
        levels.hold_r = meter.peak_hold;
        levels.clip |= meter.clip;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peak_readout_says_what_the_meter_held() {
        assert_eq!(peak_readout_text(0.0, 0.0, false), "-\u{221e}");
        assert_eq!(peak_readout_text(0.5, 0.25, false), "-6.0");
        assert_eq!(peak_readout_text(1.0, 0.2, false), "0.0");
        // Levels arrive clamped to full scale: the clip flag is the over.
        assert_eq!(peak_readout_text(1.0, 1.0, true), "CLIP");
    }

    /// A rebuild of the channel strips forgets their slots; the pinned
    /// strips, a separate view, keep theirs until they rebuild.
    #[test]
    fn strip_and_pinned_builds_clear_only_their_own_slots() {
        let bounds = Bounds::default();
        let mut layout = MeterLayout::default();
        let track = MeterSource::Track {
            id: "t1".into(),
            index: 0,
            vsti: None,
        };
        layout
            .slots
            .insert((SlotKind::Meter, track.key()), (track.clone(), bounds));
        layout.slots.insert(
            (SlotKind::Peak, MeterSource::Master.key()),
            (MeterSource::Master, bounds),
        );
        layout.begin_strips_build();
        assert_eq!(layout.slots.len(), 1);
        assert!(layout.slots.values().all(|(source, _)| source.is_pinned()));
        layout
            .slots
            .insert((SlotKind::Meter, track.key()), (track, bounds));
        layout.begin_pinned_build();
        assert!(layout.slots.values().all(|(source, _)| !source.is_pinned()));
    }
}
