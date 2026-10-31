//! An [`Editor`] implementation for GPUI.

use crossbeam::atomic::AtomicCell;
use gpui::{
    px, AnyWindowHandle, App, AsyncApp, Bounds, Entity, Render, Window, WindowBounds,
    WindowDecorations, WindowKind, WindowOptions,
};
use nih_plug::nih_error;
use nih_plug::prelude::{Editor, GuiContext, ParentWindowHandle};
use parking_lot::Mutex;
use std::any::Any;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::embed::{self, ParentHandle};
use crate::{host, EditorContext, GpuiState};

/// How long the editor waits between redraws. See [`spawn_frame_loop()`] for why the editor redraws
/// on a timer instead of only when something changed.
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// An [`Editor`] implementation that hosts a GPUI window.
pub(crate) struct GpuiEditor<V, B> {
    pub(crate) gpui_state: Arc<GpuiState>,

    /// Whether to open an independent desktop window when GPUI selects Wayland.
    pub(crate) standalone_on_wayland: bool,

    /// The user's build function, called every time the editor is opened to create the window's
    /// root view.
    pub(crate) build: Arc<B>,

    /// The scaling factor reported by the host, if any.
    pub(crate) scaling_factor: AtomicCell<Option<f32>>,

    /// `V` only shows up in `B`'s return type, so it needs to be named here for the `Editor`
    /// implementation to be able to constrain it. Using `fn() -> V` keeps this `Send + Sync`
    /// regardless of what `V` is.
    pub(crate) _root_view: PhantomData<fn() -> V>,
}

impl<V, B> Editor for GpuiEditor<V, B>
where
    V: Render,
    B: Fn(&EditorContext, &mut Window, &mut App) -> Entity<V> + 'static + Send + Sync,
{
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn Any + Send> {
        let standalone = self.standalone_on_wayland && is_wayland_backend();
        let editor_context = EditorContext {
            context,
            gpui_state: self.gpui_state.clone(),
            standalone,
        };
        let build = self.build.clone();
        let gpui_state = self.gpui_state.clone();
        let parent = ParentHandle::from(parent);

        // Mark the editor open before the window exists. Hosts such as Cubase call
        // `set_scale_factor` immediately after `attached` returns, and that has to land on the
        // window the task below is about to create.
        self.gpui_state.open.store(true, Ordering::Release);

        // The window is created on GPUI's own thread, so this handle is only filled in once that
        // has happened. Tasks run in the order they were submitted, so the task that closes the
        // window again is guaranteed to observe it.
        let window: Arc<Mutex<Option<AnyWindowHandle>>> = Arc::default();

        let window_slot = window.clone();
        let opened = host::run_on_gpui_thread(Box::new(move |cx| {
            // Read the scale here, not when `spawn` was called. The host may have reported it
            // while this task was waiting its turn.
            let scaling_factor = gpui_state.scale();
            let logical_size = gpui_state.size();
            let Some(handle) = open_window(
                cx,
                build.as_ref(),
                &editor_context,
                parent,
                logical_size,
                scaling_factor,
                standalone,
            ) else {
                return;
            };

            *window_slot.lock() = Some(handle);
            spawn_frame_loop(cx, handle, gpui_state, standalone);
        }));
        if !opened {
            self.gpui_state.open.store(false, Ordering::Release);
        }

        Box::new(GpuiEditorHandle {
            gpui_state: self.gpui_state.clone(),
            window,
        })
    }

    fn size(&self) -> (u32, u32) {
        self.gpui_state.size()
    }

    fn set_scale_factor(&self, factor: f32) -> bool {
        // Cubase reports the scale after the view is attached, and again when the window moves to
        // another monitor. Refusing the change while the editor is open leaves the GUI at 1x in
        // the corner of a 2x host window.
        if !factor.is_finite() || factor <= 0.0 {
            return false;
        }

        // A standalone Wayland window uses the compositor's scale. The host's
        // embedded-view scale does not describe that window.
        if self.standalone_on_wayland && is_wayland_backend() {
            return true;
        }

        self.scaling_factor.store(Some(factor));
        self.gpui_state.set_scale(factor);

        true
    }

    fn param_value_changed(&self, _id: &str, _normalized_value: f32) {
        // The editor redraws every frame while it is open, so there's nothing to do here
    }

    fn param_modulation_changed(&self, _id: &str, _modulation_offset: f32) {}

    fn param_values_changed(&self) {
        // Same
    }
}

/// Multiply a logical size by a scaling factor to get a physical size.
fn scale((width, height): (u32, u32), factor: f32) -> (u32, u32) {
    let factor = if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    };

    (
        (width as f32 * factor).round() as u32,
        (height as f32 * factor).round() as u32,
    )
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn is_wayland_backend() -> bool {
    gpui::guess_compositor() == "Wayland"
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn is_wayland_backend() -> bool {
    false
}

/// Open the editor as a standalone window or embed it in the host's window. Returns `None` and logs
/// an error if opening or embedding failed.
///
/// `logical_size` is the editor's size before scaling. GPUI's own geometry is in logical pixels,
/// while the platform APIs used to embed the window deal in physical ones, so the host's scaling
/// factor is applied only where it needs to be.
fn open_window<V, B>(
    cx: &mut App,
    build: &B,
    editor_context: &EditorContext,
    parent: ParentHandle,
    logical_size: (u32, u32),
    scaling_factor: f32,
    standalone: bool,
) -> Option<AnyWindowHandle>
where
    V: Render,
    B: Fn(&EditorContext, &mut Window, &mut App) -> Entity<V>,
{
    let physical_size = scale(logical_size, scaling_factor);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: gpui::point(px(0.0), px(0.0)),
            size: gpui::size(px(logical_size.0 as f32), px(logical_size.1 as f32)),
        })),
        // Embedded editors leave window chrome to the host.
        titlebar: None,
        window_decorations: Some(if standalone {
            WindowDecorations::Server
        } else {
            WindowDecorations::Client
        }),
        focus: standalone,
        show: standalone,
        kind: if standalone {
            WindowKind::Normal
        } else {
            WindowKind::PopUp
        },
        is_movable: standalone,
        is_resizable: standalone,
        is_minimizable: standalone,
        ..Default::default()
    };

    let handle = match cx.open_window(options, |window, cx| build(editor_context, window, cx)) {
        Ok(handle) => AnyWindowHandle::from(handle),
        Err(err) => {
            nih_error!("Could not open the GPUI window: {err}");
            return None;
        }
    };

    match handle.update(cx, |_, window, cx| {
        if standalone {
            window.set_window_title("WhiteSharp");
            return Ok(());
        }

        // The window's size is only the size the plugin asked for if GPUI draws it at the same
        // scaling factor the host used to compute that size, so this needs to happen first
        if let Err(err) = embed::set_scaling_factor(window, scaling_factor, physical_size) {
            nih_error!("Could not change the GPUI window's scaling factor: {err}");
        }

        let result = embed::embed(window, parent, physical_size);
        if result.is_ok() {
            resized(window, cx);
        }

        result
    }) {
        Ok(Ok(())) => Some(handle),
        Ok(Err(err)) => {
            nih_error!("Could not embed the GPUI window in the host's window: {err}");
            close_window(cx, handle);

            None
        }
        Err(err) => {
            nih_error!("The GPUI window disappeared before it could be embedded: {err}");

            None
        }
    }
}

fn close_window(cx: &mut App, handle: AnyWindowHandle) {
    let _ = handle.update(cx, |_, window, _| window.remove_window());
}

/// Let GPUI know that `window`'s native window has been resized behind its back.
///
/// Both embedding and resizing go through the platform's APIs, so GPUI only finds out about the new
/// size through its platform backend's resize callback. That callback in turn needs to update the
/// application, which it can't do while the application is already being updated. Since everything
/// this adapter does happens from within an update, the callback is always dropped and GPUI would
/// keep laying out the GUI for the size its window happened to be created with. Pulling the new size
/// in explicitly is exactly what that callback would have done.
fn resized(window: &mut Window, cx: &mut App) {
    window.bounds_changed(cx);
}

/// Redraw the editor's window on a timer, and apply any resize requested through
/// [`EditorContext::request_resize()`].
///
/// Plugin GUIs almost always contain meters or other continuously changing elements, and hosts tend
/// to open the editor's window before it is visible. Just like the other NIH-plug GUI adapters this
/// therefore redraws unconditionally instead of trying to track what changed. The loop ends by
/// itself once the window is gone.
fn spawn_frame_loop(
    cx: &mut App,
    handle: AnyWindowHandle,
    gpui_state: Arc<GpuiState>,
    standalone: bool,
) {
    cx.spawn(async move |cx: &mut AsyncApp| loop {
        let timer = cx.background_executor().timer(FRAME_INTERVAL);
        timer.await;

        let pending_scale = gpui_state.take_pending_scale();
        let scaling_factor = gpui_state.scale();
        let requested_size = gpui_state
            .requested_size
            .swap(None)
            .map(|size| scale(size, scaling_factor));

        let alive = cx.update(|cx| {
            handle
                .update(cx, |_, window, cx| {
                    if let Some(factor) = pending_scale {
                        let physical = scale(gpui_state.size(), factor);
                        match embed::set_scaling_factor(window, factor, physical) {
                            Ok(()) => resized(window, cx),
                            Err(err) => {
                                nih_error!("Could not apply the host scale factor: {err}")
                            }
                        }
                    }

                    if let Some(size) = requested_size {
                        if standalone {
                            window.resize(gpui::size(px(size.0 as f32), px(size.1 as f32)));
                            resized(window, cx);
                        } else {
                            match embed::resize(window, size) {
                                Ok(()) => resized(window, cx),
                                Err(err) => nih_error!("Could not resize the GPUI window: {err}"),
                            }
                        }
                    }

                    window.refresh();
                })
                .is_ok()
        });
        if !alive {
            break;
        }
    })
    .detach();
}

/// The handle returned from [`GpuiEditor::spawn()`]. Dropping this closes the editor's window.
struct GpuiEditorHandle {
    gpui_state: Arc<GpuiState>,
    window: Arc<Mutex<Option<AnyWindowHandle>>>,
}

impl Drop for GpuiEditorHandle {
    fn drop(&mut self) {
        self.gpui_state.open.store(false, Ordering::Release);

        let window = self.window.clone();
        host::run_on_gpui_thread(Box::new(move |cx| {
            if let Some(handle) = window.lock().take() {
                close_window(cx, handle);
            }
        }));
    }
}
