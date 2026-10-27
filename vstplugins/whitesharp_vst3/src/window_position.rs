//! Only Studio's floating-window opener reaches for this; the plug-in's
//! window is placed by the host.

use gpui::{App, Bounds, Pixels, Size, WindowOptions};

pub fn centered_window_bounds(
    _owner: Option<Bounds<Pixels>>,
    size: Size<Pixels>,
    cx: &App,
) -> Bounds<Pixels> {
    Bounds::centered(None, size, cx)
}

pub fn apply_owner_display(
    _options: &mut WindowOptions,
    _owner: Option<Bounds<Pixels>>,
    _cx: &App,
) {
}
