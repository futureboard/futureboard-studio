//! Studio's `components` module, cut down to the WhiteSharp editor and what
//! it uses. `#[path]` modules are Studio's own files; the others stand in for
//! the parts of Studio the plug-in has no use for.

#[path = "../../../../crates/SphereUIComponents/src/components/controls.rs"]
pub mod controls;
#[path = "../../../../crates/SphereUIComponents/src/components/fader.rs"]
pub mod fader;
#[path = "../../../../crates/SphereUIComponents/src/components/fx_model.rs"]
pub mod fx_model;
#[path = "../../../../crates/SphereUIComponents/src/components/knob.rs"]
pub mod knob;
pub mod plate;
#[path = "../../../../crates/SphereUIComponents/src/components/white_sharp_meter.rs"]
pub mod white_sharp_meter;
#[path = "../../../../crates/SphereUIComponents/src/components/white_sharp_model.rs"]
pub mod white_sharp_model;
#[path = "../../../../crates/SphereUIComponents/src/components/white_sharp_panel.rs"]
pub mod white_sharp_panel;
#[path = "../../../../crates/SphereUIComponents/src/components/white_sharp_stage.rs"]
pub mod white_sharp_stage;
#[path = "../../../../crates/SphereUIComponents/src/components/white_sharp_style.rs"]
pub mod white_sharp_style;
#[path = "../../../../crates/SphereUIComponents/src/components/white_sharp_window.rs"]
pub mod white_sharp_window;

pub mod builtin_plugin_editor;
pub mod builtin_plugin_editor_window;
pub mod native_plugin_shell;
pub mod quick_sampler_panel;
pub mod timeline;
pub mod title_bar;
pub mod visualizer;
