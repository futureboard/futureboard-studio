//! The built-in Slicer editor's state, callbacks and hit-testing.
//!
//! The editor is drawn by [`crate::components::slicer_plate`]; its state
//! lives in [`crate::components::slicer_window::SlicerEditorWindow`].

use std::sync::Arc;

use gpui::{App, Window};

use crate::components::quick_sampler_panel::{
    I32Cb, NoteCb, SampleSummary, VoidCb, FLAG_H, KEYBOARD_HIGHEST_ROOT,
};
use slicer::SlicerParams;

pub const SLICER_TITLE: &str = "Slicer";

/// A slice flag: wide enough for two digits.
pub(crate) const SLICE_FLAG_W: f32 = 18.0;
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
/// top-left corner across a display `width` wide: on its numbered flag, or
/// within [`GRAB_PX`] of its line. A later flag is drawn over an earlier
/// one, so it wins where they overlap.
pub fn point_under(p: &SlicerParams, x: f32, y: f32, width: f32) -> Option<usize> {
    let points = p.points();
    let line_x = |point: f32| point.clamp(0.0, 1.0) * width;
    if (0.0..=FLAG_H).contains(&y) {
        let on_flag = points.iter().rposition(|point| {
            let left = line_x(*point);
            x >= left - GRAB_PX * 0.5 && x <= left + SLICE_FLAG_W
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

pub type SlicerParamsCb = Arc<dyn Fn(&SlicerParams, &mut Window, &mut App) + 'static>;
/// A press on the display: window-space x and y, and the click count.
pub type DisplayPressCb = Arc<dyn Fn(&(f32, f32, usize), &mut Window, &mut App) + 'static>;
/// A right-press on the display: window-space x and y.
pub type DisplayRightPressCb = Arc<dyn Fn(&(f32, f32), &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct SlicerCallbacks {
    pub on_browse: VoidCb,
    /// One complete params value, sent as it is.
    pub on_set_params: SlicerParamsCb,
    /// Params whose slicing settings changed: the window cuts the sample
    /// again with them.
    pub on_reslice: SlicerParamsCb,
    pub on_display_press: DisplayPressCb,
    pub on_display_right_press: DisplayRightPressCb,
    pub on_note_on: NoteCb,
    pub on_note_off: NoteCb,
    pub on_shift_octave: I32Cb,
}

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
        assert_eq!(point_under(&p, 14.0, 6.0, 1_000.0), Some(0));
        // Two overlapping flags: the one drawn on top.
        assert_eq!(point_under(&p, 512.0, 6.0, 1_000.0), Some(2));
        // Below the flags only the line counts.
        assert_eq!(point_under(&p, 14.0, 80.0, 1_000.0), None);
        assert_eq!(point_under(&p, 497.0, 80.0, 1_000.0), Some(1));
        assert_eq!(point_under(&p, 300.0, 80.0, 1_000.0), None);
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
