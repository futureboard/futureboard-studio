//! Studio's `components` module, cut down to the Voxtune editor and the
//! controls it uses. `#[path]` modules are Studio's own files; `timeline`
//! stands in for the mixer state the fader reads, and mounts Studio's
//! playhead helpers.

#[path = "../../../../crates/SphereUIComponents/src/components/controls.rs"]
pub mod controls;
#[path = "../../../../crates/SphereUIComponents/src/components/drag_pointer.rs"]
pub mod drag_pointer;
#[path = "../../../../crates/SphereUIComponents/src/components/fader.rs"]
pub mod fader;
#[path = "../../../../crates/SphereUIComponents/src/components/slider.rs"]
pub mod slider;
#[path = "../../../../crates/SphereUIComponents/src/components/voxtune/mod.rs"]
pub mod voxtune;

pub mod timeline;
