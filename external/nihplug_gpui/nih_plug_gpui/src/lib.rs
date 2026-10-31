//! [GPUI](https://gpui.rs) editor support for NIH-plug.
//!
//! This uses the [GPUI Standalone Edition](https://github.com/futureboard/gpui-se) fork, which
//! packages GPUI as a set of independent crates instead of as part of the Zed editor.
//!
//! Check out the `gain_gui_gpui` example for a complete plugin using this adapter. In short, you
//! write a regular GPUI [`Render`] implementation and hand it to [`create_gpui_editor()`]:
//!
//! ```ignore
//! struct GainEditor {
//!     context: EditorContext,
//!     params: Arc<GainParams>,
//! }
//!
//! impl Render for GainEditor {
//!     fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
//!         let setter = self.context.setter();
//!         div().child(format!("{}", self.params.gain))
//!     }
//! }
//!
//! create_gpui_editor(
//!     self.params.editor_state.clone(),
//!     move |context, _window, cx| {
//!         let context = context.clone();
//!         let params = params.clone();
//!         cx.new(|_| GainEditor { context, params })
//!     },
//! )
//! ```
//!
//! # Platform support
//!
//! GPUI owns its event loop and always creates top level windows, so this adapter runs a single
//! process-wide GPUI application on its own thread and reparents the windows it creates into the
//! window supplied by the plugin host. That works on Windows and on X11, but not on macOS, where
//! GPUI must drive `NSApplication` on the process' main thread that the host already owns. Opening
//! an editor on macOS logs an error and does nothing. See the `host` and `embed` modules for the
//! details.

// See the comment in the main `nih_plug` crate
#![allow(clippy::type_complexity)]

use crossbeam::atomic::AtomicCell;
use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::{Editor, GuiContext, ParamSetter};
use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Re-export for convenience.
pub use gpui;

use gpui::{App, Entity, Render, Window};

mod editor;
mod embed;
mod host;

/// Create an [`Editor`] instance using a [GPUI](gpui) GUI.
///
/// `build` is called on GPUI's own thread every time the editor is opened, and should return the
/// root view for the editor's window. It is passed an [`EditorContext`], which is how the GUI reads
/// and writes parameters. Because the editor may be opened from any thread and GPUI runs on its
/// own, `build` needs to be `Send + Sync`.
///
/// The [`GpuiState`] passed to this function contains the GUI's initial size, and it is kept in
/// sync whenever the GUI gets resized. You can also use it to check whether the GUI is open so you
/// can avoid performing potentially expensive calculations while it is closed. If you want the size
/// to be persisted when restoring a plugin instance, then store it in a `#[persist = "key"]` field
/// on your parameters struct.
///
/// See [`GpuiState::from_size()`].
pub fn create_gpui_editor<V, B>(gpui_state: Arc<GpuiState>, build: B) -> Option<Box<dyn Editor>>
where
    V: Render,
    B: Fn(&EditorContext, &mut Window, &mut App) -> Entity<V> + 'static + Send + Sync,
{
    create_gpui_editor_inner(gpui_state, build, false)
}

/// Create a GPUI editor that opens as a separate desktop window on Wayland.
///
/// Wayland does not allow a plugin to reparent its window into the host's
/// window. When GPUI selects the Wayland backend, this editor therefore uses
/// a normal titled window and resizes it directly. On X11 and Windows it keeps
/// the usual embedded editor behavior.
pub fn create_gpui_editor_with_wayland_fallback<V, B>(
    gpui_state: Arc<GpuiState>,
    build: B,
) -> Option<Box<dyn Editor>>
where
    V: Render,
    B: Fn(&EditorContext, &mut Window, &mut App) -> Entity<V> + 'static + Send + Sync,
{
    create_gpui_editor_inner(gpui_state, build, true)
}

fn create_gpui_editor_inner<V, B>(
    gpui_state: Arc<GpuiState>,
    build: B,
    standalone_on_wayland: bool,
) -> Option<Box<dyn Editor>>
where
    V: Render,
    B: Fn(&EditorContext, &mut Window, &mut App) -> Entity<V> + 'static + Send + Sync,
{
    Some(Box::new(editor::GpuiEditor {
        gpui_state,
        build: Arc::new(build),
        scaling_factor: AtomicCell::new(None),
        standalone_on_wayland,
        _root_view: PhantomData,
    }))
}

/// Install the [`AssetSource`](gpui::AssetSource) GPUI resolves `svg()` and `img()` paths through.
///
/// Every editor in the process shares one GPUI application, so this has to be called before the
/// first editor is opened, typically from `Plugin::editor()` right before
/// [`create_gpui_editor()`]. Returns `false` if a source was already installed or the application
/// is already running, in which case `source` is not used.
pub fn set_asset_source(source: impl gpui::AssetSource) -> bool {
    host::set_asset_source(Arc::new(source))
}

/// Everything the GUI needs to interact with the plugin. This is handed to the `build` closure
/// passed to [`create_gpui_editor()`], and it's cheap to clone into your views.
#[derive(Clone)]
pub struct EditorContext {
    context: Arc<dyn GuiContext>,
    gpui_state: Arc<GpuiState>,
    standalone: bool,
}

impl EditorContext {
    /// A [`ParamSetter`] for changing parameter values from the GUI. Changes made through this are
    /// broadcast to the host and reflected in your `Params` struct.
    pub fn setter(&self) -> ParamSetter<'_> {
        ParamSetter::new(self.context.as_ref())
    }

    /// The raw [`GuiContext`], for the few things [`ParamSetter`] doesn't cover.
    pub fn gui_context(&self) -> &Arc<dyn GuiContext> {
        &self.context
    }

    /// The editor's [`GpuiState`].
    pub fn state(&self) -> &Arc<GpuiState> {
        &self.gpui_state
    }

    /// Ask the host to resize the editor to `(width, height)` logical pixels. Returns `false` if the
    /// host refused, in which case nothing changes. The window is resized on GPUI's thread shortly
    /// after this returns.
    pub fn request_resize(&self, width: u32, height: u32) -> bool {
        if self.standalone {
            self.gpui_state.size.store((width, height));
            self.gpui_state.requested_size.store(Some((width, height)));
            return true;
        }

        // The host asks the editor for its new size while handling this request, so the new size
        // needs to be in place before asking and it needs to be rolled back if the host refuses
        let old_size = self.gpui_state.size.swap((width, height));
        if !self.context.request_resize() {
            self.gpui_state.size.store(old_size);

            return false;
        }

        self.gpui_state.requested_size.store(Some((width, height)));

        true
    }
}

/// State for an `nih_plug_gpui` editor.
#[derive(Debug, Serialize, Deserialize)]
pub struct GpuiState {
    /// The window's size in logical pixels before applying `scale_factor`.
    #[serde(with = "nih_plug::params::persist::serialize_atomic_cell")]
    size: AtomicCell<(u32, u32)>,

    /// The new size of the window, if it was requested to resize by the GUI. This is consumed by
    /// the editor once it has resized its window.
    #[serde(skip)]
    requested_size: AtomicCell<Option<(u32, u32)>>,

    /// The host's content scale. Logical sizes are multiplied by this to get physical pixels.
    ///
    /// Not persisted: the host reports it again for every editor. `1.0` when the host has not
    /// reported one. [`default_scale`] keeps a restored instance from deserializing this as `0`.
    #[serde(skip, default = "default_scale")]
    scale: AtomicCell<f32>,

    /// `scale` changed while the editor was open and the native window still has to apply it.
    #[serde(skip)]
    scale_dirty: AtomicBool,

    /// Whether the editor's window is currently open.
    #[serde(skip)]
    open: AtomicBool,
}

fn default_scale() -> AtomicCell<f32> {
    AtomicCell::new(1.0)
}

impl<'a> PersistentField<'a, GpuiState> for Arc<GpuiState> {
    fn set(&self, new_value: GpuiState) {
        self.size.store(new_value.size.load());
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&GpuiState) -> R,
    {
        f(self)
    }
}

impl GpuiState {
    /// Initialize the GUI's state. This value can be passed to [`create_gpui_editor()`]. The window
    /// size is in logical pixels, so before it is multiplied by the DPI scaling factor.
    pub fn from_size(width: u32, height: u32) -> Arc<GpuiState> {
        Arc::new(GpuiState {
            size: AtomicCell::new((width, height)),
            requested_size: AtomicCell::new(None),
            scale: AtomicCell::new(1.0),
            scale_dirty: AtomicBool::new(false),
            open: AtomicBool::new(false),
        })
    }

    /// The content scale last reported by the host. `1.0` until the host sets one.
    pub(crate) fn scale(&self) -> f32 {
        let scale = self.scale.load();
        if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        }
    }

    /// Remember `factor`. When the editor is open the native window still has the previous scale,
    /// so the frame loop must apply it.
    pub(crate) fn set_scale(&self, factor: f32) {
        self.scale.store(factor);
        if self.is_open() {
            self.scale_dirty.store(true, Ordering::Release);
        }
    }

    /// The scale to apply to the native window, if the host changed it since the last frame.
    pub(crate) fn take_pending_scale(&self) -> Option<f32> {
        if self.scale_dirty.swap(false, Ordering::AcqRel) {
            Some(self.scale())
        } else {
            None
        }
    }

    /// Returns a `(width, height)` pair for the current size of the GUI in logical pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size.load()
    }

    /// Whether the GUI is currently visible.
    // Called `is_open()` instead of `open()` to avoid the ambiguity.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }
}
