//! The piano roll's playhead, as its own GPUI entity.
//!
//! The same problem the arrangement had, and the same answer — see
//! `components/timeline/playhead.rs`, which this deliberately mirrors.
//!
//! The line moves on every playback tick, up to the display refresh rate. Drawn
//! inside `PianoRoll::render` it cost a full rebuild of the editor for a
//! one-pixel translation: every note, every grid line, every CC lane, the
//! keyboard column and the ruler, none of which had changed. GPUI invalidates
//! per entity, so the fix is an entity — notifying this one repaints the line
//! and nothing else.
//!
//! In the floating MIDI editor the symptom was worse than cost. Nothing
//! notified that window on a playback tick at all, so its playhead only moved
//! when something unrelated happened to repaint it: it jumped in visible steps
//! rather than sweeping. Publishing to this overlay from the audio poll is what
//! makes it move at all, and doing it here is what keeps it cheap.

use gpui::{
    canvas, div, fill, px, size, Bounds, Context, IntoElement, ParentElement, Pixels, Render,
    Styled, Window,
};

use crate::theme::Colors;

/// Where the piano roll's playhead is this frame, shared between the roll that
/// computes it and the overlay that draws it.
///
/// A cell rather than an entity field so `PianoRoll::render` can refresh it
/// without leasing the overlay: the roll recomputes x whenever it lays itself
/// out (a scroll, a zoom, a resize, a clip change), and the playback poll
/// recomputes it between those.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PianoRollPlayheadFrame {
    /// x within the note grid, in the same space `beat_to_x` returns.
    pub x: f32,
    /// Whether the playhead is inside the clip being edited at all. Outside it
    /// there is nothing to draw — the transport is somewhere else in the song.
    pub visible: bool,
    /// Transport state, which is the difference between a moving playhead and a
    /// parked marker, and is drawn as a difference in weight.
    pub playing: bool,
}

pub type PianoRollPlayheadFrameCell = std::rc::Rc<std::cell::Cell<PianoRollPlayheadFrame>>;

/// Draws the line, and only the line.
pub struct PianoRollPlayheadOverlay {
    frame: PianoRollPlayheadFrameCell,
}

impl PianoRollPlayheadOverlay {
    pub fn new(frame: PianoRollPlayheadFrameCell) -> Self {
        Self { frame }
    }
}

impl Render for PianoRollPlayheadOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("PianoRollPlayheadOverlay");
        crate::perf::count("piano_roll_playhead_paint_count", 1);

        let frame = self.frame.get();
        let color = Colors::with_alpha(
            Colors::status_warning(),
            if frame.playing { 0.9 } else { 0.45 },
        );
        let x = frame.x;
        let visible = frame.visible;

        // A canvas rather than a positioned div: the div's left edge is layout,
        // so moving it re-lays-out this element's subtree on every tick, while
        // a paint closure reads the new x and draws. It also lets the line be
        // clipped against the real grid width without a wrapper that has to
        // know the geometry.
        let line = canvas(
            |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                if !visible {
                    return;
                }
                let w: f32 = bounds.size.width.into();
                let h: f32 = bounds.size.height.into();
                if h <= 0.0 || x < -2.0 || x > w + 2.0 {
                    return;
                }
                window.paint_layer(bounds, |window| {
                    let line_bounds = Bounds::new(
                        bounds.origin + gpui::point(px(x), px(0.0)),
                        size(px(1.0), px(h)),
                    );
                    window.paint_quad(fill(line_bounds, color));
                });
            },
        )
        .absolute()
        .inset_0()
        .into_any_element();

        div().absolute().inset_0().child(line)
    }
}
