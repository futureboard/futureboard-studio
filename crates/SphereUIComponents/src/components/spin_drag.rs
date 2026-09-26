//! Shared vertical scrub gesture for compact numeric readouts.
//!
//! The anchor lives in the active GPUI drag payload, rather than in a render
//! closure. This is important because changing the scrubbed value rerenders
//! the control while the pointer is still down.

use std::sync::{Arc, Mutex};

use gpui::{Empty, IntoElement, Render, Window};

#[derive(Clone, Debug)]
pub(crate) struct SpinDrag {
    id: String,
    start_value: f64,
    anchor_y: Arc<Mutex<Option<f32>>>,
    /// The anchor of a scrub whose scale can change mid-drag; see
    /// [`Self::value_at_rescaled`].
    rescaled: Arc<Mutex<Option<SpinRescale>>>,
}

/// Where a rescalable scrub measures from: the value and pointer y at the last
/// change of scale, and the value it last reported.
#[derive(Clone, Copy, Debug)]
struct SpinRescale {
    anchor_y: f32,
    base_value: f64,
    units_per_pixel: f64,
    last_value: f64,
}

impl SpinDrag {
    pub(crate) fn new(id: impl Into<String>, start_value: f64) -> Self {
        Self {
            id: id.into(),
            start_value,
            anchor_y: Arc::new(Mutex::new(None)),
            rescaled: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn begin(&self) {
        *self
            .anchor_y
            .lock()
            .expect("spin drag anchor mutex poisoned") = None;
        *self
            .rescaled
            .lock()
            .expect("spin drag anchor mutex poisoned") = None;
    }

    pub(crate) fn matches(&self, id: &str) -> bool {
        self.id == id
    }

    pub(crate) fn value_at(
        &self,
        current_y: f32,
        units_per_pixel: f64,
        min: f64,
        max: f64,
        quantum: Option<f64>,
    ) -> f64 {
        let mut anchor = self
            .anchor_y
            .lock()
            .expect("spin drag anchor mutex poisoned");
        let start_y = *anchor.get_or_insert(current_y);
        let mut delta = f64::from(start_y - current_y) * units_per_pixel;
        if let Some(quantum) = quantum.filter(|quantum| *quantum > 0.0) {
            delta = (delta / quantum).round() * quantum;
        }
        (self.start_value + delta).clamp(min, max)
    }

    /// [`Self::value_at`] for a scrub whose scale may change mid-drag (a
    /// modifier for coarse steps): the value reached so far becomes the new
    /// base where the scale changed, so switching never makes the value jump.
    pub(crate) fn value_at_rescaled(
        &self,
        current_y: f32,
        units_per_pixel: f64,
        min: f64,
        max: f64,
        quantum: Option<f64>,
    ) -> f64 {
        let mut rescaled = self
            .rescaled
            .lock()
            .expect("spin drag anchor mutex poisoned");
        let anchor = rescaled.get_or_insert(SpinRescale {
            anchor_y: current_y,
            base_value: self.start_value,
            units_per_pixel,
            last_value: self.start_value,
        });
        if (anchor.units_per_pixel - units_per_pixel).abs() > f64::EPSILON {
            anchor.anchor_y = current_y;
            anchor.base_value = anchor.last_value;
            anchor.units_per_pixel = units_per_pixel;
        }
        let mut delta = f64::from(anchor.anchor_y - current_y) * units_per_pixel;
        if let Some(quantum) = quantum.filter(|quantum| *quantum > 0.0) {
            delta = (delta / quantum).round() * quantum;
        }
        // Not `f64::clamp`: the fade steppers' `max` is what the clip's other
        // fade leaves, computed per render, and crossed bounds must not panic.
        let value = (anchor.base_value + delta).max(min).min(max.max(min));
        anchor.last_value = value;
        value
    }
}

impl Render for SpinDrag {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        Empty
    }
}

#[cfg(test)]
mod tests {
    use super::SpinDrag;

    #[test]
    fn anchor_survives_payload_clones_across_renders() {
        let drag = SpinDrag::new("gain", 10.0);
        drag.begin();
        assert_eq!(drag.value_at(100.0, 0.2, 0.0, 20.0, None), 10.0);

        let rerendered_handler_payload = drag.clone();
        assert_eq!(
            rerendered_handler_payload.value_at(75.0, 0.2, 0.0, 20.0, None),
            15.0
        );
    }

    /// Switching to coarse steps mid-scrub carries on from the value reached,
    /// in both directions.
    #[test]
    fn a_change_of_scale_mid_scrub_does_not_jump() {
        let drag = SpinDrag::new("fade", 100.0);
        drag.begin();
        assert_eq!(
            drag.value_at_rescaled(50.0, 1.0, 0.0, 10_000.0, Some(5.0)),
            100.0
        );
        assert_eq!(
            drag.value_at_rescaled(40.0, 1.0, 0.0, 10_000.0, Some(5.0)),
            110.0
        );
        // Coarse from here: 20 per pixel, still 110 at the switch.
        assert_eq!(
            drag.value_at_rescaled(40.0, 20.0, 0.0, 10_000.0, Some(100.0)),
            110.0
        );
        assert_eq!(
            drag.value_at_rescaled(30.0, 20.0, 0.0, 10_000.0, Some(100.0)),
            310.0
        );
        // And fine again, from 310.
        assert_eq!(
            drag.value_at_rescaled(30.0, 1.0, 0.0, 10_000.0, Some(5.0)),
            310.0
        );
        assert_eq!(
            drag.value_at_rescaled(35.0, 1.0, 0.0, 10_000.0, Some(5.0)),
            305.0
        );
        // Coarse again from 305, and still clamped to the range.
        assert_eq!(
            drag.value_at_rescaled(35.0, 20.0, 0.0, 400.0, Some(100.0)),
            305.0
        );
        assert_eq!(
            drag.value_at_rescaled(-900.0, 20.0, 0.0, 400.0, Some(100.0)),
            400.0
        );
        // A range that crossed (no room left for the fade) holds at its
        // floor instead of panicking.
        assert_eq!(
            drag.value_at_rescaled(-900.0, 20.0, 0.0, -0.001, Some(100.0)),
            0.0
        );
    }

    #[test]
    fn quantizes_and_clamps_scrubbed_values() {
        let drag = SpinDrag::new("stepper", 10.3);
        drag.begin();
        assert_eq!(drag.value_at(50.0, 0.2, 0.0, 20.0, Some(1.0)), 10.3);
        assert_eq!(drag.value_at(45.0, 0.2, 0.0, 20.0, Some(1.0)), 11.3);
        assert_eq!(drag.value_at(-200.0, 0.2, 0.0, 20.0, Some(1.0)), 20.0);
    }
}
