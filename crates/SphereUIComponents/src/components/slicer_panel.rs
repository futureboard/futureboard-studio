//! The built-in Slicer editor's state, callbacks and hit-testing.
//!
//! The editor is drawn by [`crate::components::slicer_plate`]; its state
//! lives in [`crate::components::slicer_window::SlicerEditorWindow`].

use std::sync::Arc;

use gpui::{App, Window};

use crate::components::quick_sampler_panel::{
    I32Cb, NoteCb, NoteVelocityCb, SampleSummary, VoidCb, KEYBOARD_HIGHEST_ROOT,
};
use slicer::SlicerParams;

pub const SLICER_TITLE: &str = "Slicer";

/// The numbered markers' bar along the top of the display, at the design
/// size.
pub const SLICE_MARKER_H: f32 = 22.0;

/// A slice marker's width for its number, at the design size.
pub fn marker_width(index: usize) -> f32 {
    let digits = (index + 1).to_string().len() as f32;
    (8.0 + 6.6 * digits).max(18.0)
}
/// How near a press must land to a flag's line to grab it, in pixels.
pub const GRAB_PX: f32 = 8.0;
/// A slice sounding in the plug-in, from whatever played it — the editor,
/// the track, the virtual keyboard — as its telemetry reports it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SoundingSlice {
    pub note: u8,
    /// Where it is reading, as a fraction of the sample.
    pub position: f32,
    /// 1 as it starts, falling to 0 a moment later: the flash over its
    /// region.
    pub flash: f32,
}

/// The slice point under a pointer at `(x, y)`, measured from the display's
/// top-left corner across a display `width` wide drawn at scale `k`: on its
/// numbered marker, or within [`GRAB_PX`] of its line. A later marker is
/// drawn over an earlier one, so it wins where they overlap.
pub fn point_under(p: &SlicerParams, x: f32, y: f32, width: f32, k: f32) -> Option<usize> {
    let points = p.points();
    let line_x = |point: f32| point.clamp(0.0, 1.0) * width;
    if (0.0..=SLICE_MARKER_H * k).contains(&y) {
        let on_flag = points.iter().enumerate().rposition(|(index, point)| {
            let left = line_x(*point);
            x >= left - GRAB_PX * 0.5 && x <= left + marker_width(index) * k
        });
        if on_flag.is_some() {
            return on_flag;
        }
    }
    points
        .iter()
        .enumerate()
        .map(|(index, point)| (index, (line_x(*point) - x).abs()))
        .filter(|(_, distance)| *distance <= GRAB_PX)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

/// What the panel draws: the insert's slicer, the loaded sample's summary,
/// and the window's transient state.
#[derive(Clone)]
pub struct SlicerPanelState {
    /// The insert's params — what its DSP in the plug-in host plays with.
    pub params: SlicerParams,
    pub sample: Option<SampleSummary>,
    pub loading: bool,
    /// Finding the sample's hits or its tempo.
    pub analysing: bool,
    pub status: Option<String>,
    pub keyboard_root: u8,
    pub active_notes: Vec<u8>,
    /// The slice point being dragged.
    pub dragging: Option<usize>,
    /// The slice last played or picked, highlighted on the display.
    pub selected: Option<usize>,
    /// Slices sounding in the plug-in, for the display and the keyboard.
    pub sounding: Vec<SoundingSlice>,
}

impl Default for SlicerPanelState {
    fn default() -> Self {
        Self {
            params: SlicerParams::default(),
            sample: None,
            loading: false,
            analysing: false,
            status: None,
            keyboard_root: slicer::DEFAULT_FIRST_KEY,
            active_notes: Vec::new(),
            dragging: None,
            selected: None,
            sounding: Vec::new(),
        }
    }
}

impl SlicerPanelState {
    pub fn is_playable(&self) -> bool {
        self.sample.is_some() && !self.loading
    }

    pub fn shift_keyboard_octave(&mut self, delta: i32) {
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root = root.clamp(0, KEYBOARD_HIGHEST_ROOT as i32) as u8;
    }
}

/// A press on the display: window-space x and y, and the click count.
pub type DisplayPressCb = Arc<dyn Fn(&(f32, f32, usize), &mut Window, &mut App) + 'static>;
/// A right-press on the display: window-space x and y.
pub type DisplayRightPressCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct SlicerCallbacks {
    pub on_browse: VoidCb,
    pub on_display_press: DisplayPressCb,
    pub on_display_right_press: DisplayRightPressCb,
    /// A key or pad pressed: pitch and velocity.
    pub on_note_on: NoteVelocityCb,
    pub on_note_off: NoteCb,
    /// The previous (−1) or next (+1) sample in the plug-in's Samples folder.
    pub on_step_sample: I32Cb,
    /// Bypass the insert, or bring it back.
    pub on_bypass: VoidCb,
    /// The Slices (0) or Voice (1) page.
    pub on_page: UsizeCb,
    /// The pads' page: slices 1–16, 17–32, …
    pub on_pad_page: UsizeCb,
}

pub type UsizeCb = Arc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::soundfont_player_mdi::note_label;

    fn edited(p: SlicerParams, apply: impl FnOnce(&mut SlicerParams)) -> SlicerParams {
        let mut next = p;
        apply(&mut next);
        next.sanitized()
    }

    #[test]
    fn the_keyboard_opens_on_the_first_slice() {
        let panel = SlicerPanelState::default();
        assert_eq!(panel.keyboard_root, panel.params.first_key);
        assert_eq!(note_label(panel.params.first_key), "C3");
    }

    #[test]
    fn a_press_on_a_flag_or_near_its_line_finds_that_slice_point() {
        let mut p = SlicerParams {
            slice_count: 3,
            ..SlicerParams::default()
        };
        p.slices[..3].copy_from_slice(&[0.0, 0.5, 0.505]);
        // On the number, well right of the line.
        assert_eq!(point_under(&p, 14.0, 6.0, 1_000.0, 1.0), Some(0));
        // Two overlapping markers: the one drawn on top.
        assert_eq!(point_under(&p, 512.0, 6.0, 1_000.0, 1.0), Some(2));
        // Below the markers only the line counts.
        assert_eq!(point_under(&p, 14.0, 80.0, 1_000.0, 1.0), None);
        assert_eq!(point_under(&p, 497.0, 80.0, 1_000.0, 1.0), Some(1));
        assert_eq!(point_under(&p, 300.0, 80.0, 1_000.0, 1.0), None);
        // Drawn at twice the size, the marker bar is twice as tall.
        assert_eq!(point_under(&p, 14.0, 40.0, 1_000.0, 2.0), Some(0));
    }

    #[test]
    fn edits_stay_in_range() {
        let p = SlicerParams::default();
        assert_eq!(edited(p, |p| p.sensitivity += 2.0).sensitivity, 1.0);
        assert_eq!(
            edited(p, |p| p.equal_slices = 0).equal_slices,
            slicer::MIN_EQUAL_SLICES
        );
    }
}
