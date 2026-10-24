//! The platform frame for dialogs.
//!
//! On Windows a dialog opened through [`open_dialog_window`] takes the OS
//! frame: the system draws its caption,
//! caption buttons and border, and the dialog's drawn titlebar
//! ([`crate::components::title_bar`]) lays out as nothing, handing its title
//! to the caption and its close action to the caption's close button (and
//! Alt+F4). Linux and macOS keep the drawn chrome.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    px, AnyElement, App, Display, Element, ElementId, Entity, GlobalElementId, IntoElement,
    LayoutId, Render, Style, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowId,
    WindowOptions,
};

use crate::platform_chrome::{
    PlatformChromeKind, PlatformChromePolicy, APP_WINDOW_TITLE, TITLEBAR_HEIGHT_PX,
};

/// Whether dialogs take the platform's own frame. Windows only.
pub fn native_dialog_frame() -> bool {
    PlatformChromePolicy::current().kind == PlatformChromeKind::Windows
}

/// What the caption's close button does for a native-framed dialog.
#[derive(Clone)]
pub(crate) enum DialogClose {
    /// Runs the dialog's own close action, which removes the window itself.
    Callback(Rc<dyn Fn(&mut Window, &mut App)>),
    /// Nothing may close the window right now (a session transaction).
    Blocked,
}

#[derive(Default)]
struct NativeDialog {
    title: Option<String>,
    close: Option<DialogClose>,
}

thread_local! {
    static NATIVE_DIALOGS: RefCell<HashMap<WindowId, NativeDialog>> =
        RefCell::new(HashMap::new());
}

/// `cx.open_window` for a dialog: an owned `WindowKind::Dialog`, or a
/// standalone one (`WindowKind::Normal`) at startup. On Windows it gets the
/// platform frame ([`native_dialog_frame`]); its client area loses the drawn
/// titlebar's height, which the OS caption now provides outside it. While the
/// loading-session window is up, the dialog is kept above it.
pub fn open_dialog_window<V: 'static + Render>(
    cx: &mut App,
    mut options: WindowOptions,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> gpui::Result<WindowHandle<V>> {
    let native = native_dialog_frame();
    if native {
        apply_native_frame(&mut options);
    }
    let above_loading = crate::loading_session::is_loading_session_window_open(cx);
    cx.open_window(options, move |window, cx| {
        if above_loading {
            keep_on_top(window);
        }
        if !native {
            return build(window, cx);
        }
        let id = window.window_handle().window_id();
        NATIVE_DIALOGS.with(|dialogs| dialogs.borrow_mut().insert(id, NativeDialog::default()));
        theme_frame(window);
        window.on_window_should_close(cx, move |window, cx| {
            let close = NATIVE_DIALOGS.with(|dialogs| {
                dialogs
                    .borrow()
                    .get(&id)
                    .and_then(|dialog| dialog.close.clone())
            });
            match close {
                // No drawn titlebar has said how to close it: the default.
                None => true,
                Some(DialogClose::Blocked) => false,
                Some(DialogClose::Callback(close)) => {
                    close(window, cx);
                    false
                }
            }
        });
        let view = build(window, cx);
        cx.observe_release(&view, move |_, _| {
            NATIVE_DIALOGS.with(|dialogs| dialogs.borrow_mut().remove(&id));
        })
        .detach();
        view
    })
}

fn apply_native_frame(options: &mut WindowOptions) {
    options.titlebar = Some(TitlebarOptions {
        title: Some(APP_WINDOW_TITLE.into()),
        appears_transparent: false,
        traffic_light_position: None,
    });
    options.window_decorations = None;
    if let Some(WindowBounds::Windowed(bounds)) = options.window_bounds.as_mut() {
        bounds.size.height = (bounds.size.height - px(TITLEBAR_HEIGHT_PX)).max(px(1.0));
    }
    if let Some(min) = options.window_min_size.as_mut() {
        min.height = (min.height - px(TITLEBAR_HEIGHT_PX)).max(px(1.0));
    }
}

/// A dialog's drawn titlebar. In a native-framed dialog it lays out as
/// nothing and hands its title and close action to the platform caption;
/// anywhere else it draws `bar`.
pub(crate) struct DialogTitlebar {
    bar: AnyElement,
    title: String,
    close: DialogClose,
}

impl DialogTitlebar {
    pub(crate) fn new(bar: AnyElement, title: String, close: DialogClose) -> Self {
        Self { bar, title, close }
    }
}

impl IntoElement for DialogTitlebar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for DialogTitlebar {
    /// Whether `bar` is drawn.
    type RequestLayoutState = bool;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let id = window.window_handle().window_id();
        let title_changed = NATIVE_DIALOGS.with(|dialogs| {
            let mut dialogs = dialogs.borrow_mut();
            let dialog = dialogs.get_mut(&id)?;
            dialog.close = Some(self.close.clone());
            let changed = dialog.title.as_deref() != Some(self.title.as_str());
            if changed {
                dialog.title = Some(self.title.clone());
            }
            Some(changed)
        });
        match title_changed {
            Some(changed) => {
                if changed {
                    window.set_window_title(&self.title);
                }
                let style = Style {
                    display: Display::None,
                    ..Style::default()
                };
                (window.request_layout(style, [], cx), false)
            }
            None => (self.bar.request_layout(window, cx), true),
        }
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        drawn: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if *drawn {
            self.bar.prepaint(window, cx);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        drawn: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if *drawn {
            self.bar.paint(window, cx);
        }
    }
}

#[cfg(target_os = "windows")]
fn hwnd_of(window: &Window) -> Option<windows::Win32::Foundation::HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(raw) => {
            Some(windows::Win32::Foundation::HWND(raw.hwnd.get() as *mut _))
        }
        _ => None,
    }
}

/// Keeps `window` above every other window, the app's own included, for as
/// long as it is open. Windows only; macOS already holds dialogs at the
/// modal-panel level.
pub fn keep_on_top(window: &Window) {
    #[cfg(target_os = "windows")]
    if let Some(hwnd) = hwnd_of(window) {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        };
        // SAFETY: a Win32 call on a live window this thread owns.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = window;
}

/// The OS frame in the app theme's appearance rather than the system's.
fn theme_frame(window: &Window) {
    #[cfg(target_os = "windows")]
    if let Some(hwnd) = hwnd_of(window) {
        use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
        let dark: i32 = crate::theme::is_dark().into();
        // SAFETY: a Win32 call on a live window this thread owns; `dark`
        // outlives it.
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                (&dark as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = window;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size, Bounds, WindowKind};

    #[test]
    fn the_native_frame_takes_the_drawn_titlebar_out_of_the_client_area() {
        let mut options = WindowOptions {
            kind: WindowKind::Dialog,
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(10.0), px(20.0)),
                size: size(px(400.0), px(300.0)),
            })),
            window_min_size: Some(size(px(200.0), px(150.0))),
            ..Default::default()
        };
        apply_native_frame(&mut options);
        let Some(WindowBounds::Windowed(bounds)) = options.window_bounds else {
            panic!("bounds stay windowed");
        };
        assert_eq!(bounds.size, size(px(400.0), px(300.0 - TITLEBAR_HEIGHT_PX)));
        assert_eq!(bounds.origin, point(px(10.0), px(20.0)));
        assert_eq!(
            options.window_min_size,
            Some(size(px(200.0), px(150.0 - TITLEBAR_HEIGHT_PX)))
        );
        let titlebar = options.titlebar.expect("a platform caption");
        assert!(!titlebar.appears_transparent);
        assert!(options.window_decorations.is_none());
    }
}
