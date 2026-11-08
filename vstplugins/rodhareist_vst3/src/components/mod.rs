//! Studio's `components` module, cut down to the Rodhareist editor and what
//! it uses. `#[path]` modules are Studio's own files; the others stand in for
//! the parts of Studio the plug-in has no use for.

#[path = "../../../../crates/SphereUIComponents/src/components/builtin_plugin_files.rs"]
pub mod builtin_plugin_files;
#[path = "../../../../crates/SphereUIComponents/src/components/controls.rs"]
pub mod controls;
#[path = "../../../../crates/SphereUIComponents/src/components/drag_pointer.rs"]
pub mod drag_pointer;
#[path = "../../../../crates/SphereUIComponents/src/components/fader.rs"]
pub mod fader;
#[path = "../../../../crates/SphereUIComponents/src/components/plate.rs"]
pub mod plate;
#[path = "../../../../crates/SphereUIComponents/src/components/rodhareist_blocks.rs"]
pub mod rodhareist_blocks;
#[path = "../../../../crates/SphereUIComponents/src/components/rodhareist_panel.rs"]
pub mod rodhareist_panel;
#[path = "../../../../crates/SphereUIComponents/src/components/rodhareist_window.rs"]
pub mod rodhareist_window;
#[path = "../../../../crates/SphereUIComponents/src/components/text_input.rs"]
pub mod text_input;

pub mod builtin_plugin_editor;
pub mod context_menu;
pub mod builtin_plugin_editor_window;
pub mod native_plugin_shell;
pub mod plugin_kit;
pub mod timeline;
pub mod title_bar;
