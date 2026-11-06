use gpui::{px, size, App, Bounds, Point, WindowBounds, WindowOptions};

use sphere_ui_components::platform_chrome;
use sphere_ui_components::window_position;

pub const WELCOME_WIDTH: f32 = 1180.0;
pub const WELCOME_HEIGHT: f32 = 820.0;

/// Main Futureboard Studio workspace window — centered or restored from disk.
pub fn studio_window_options(cx: &mut App) -> WindowOptions {
    window_position::studio_window_options(cx)
}

pub fn welcome_window_options(cx: &App) -> WindowOptions {
    let welcome_size = size(px(WELCOME_WIDTH), px(WELCOME_HEIGHT));
    let origin = cx
        .primary_display()
        .map(|display| {
            let b = display.bounds();
            let ox = f32::from(b.origin.x);
            let oy = f32::from(b.origin.y);
            let dw = f32::from(b.size.width);
            let dh = f32::from(b.size.height);
            Point {
                x: px(ox + (dw - WELCOME_WIDTH).max(0.0) / 2.0),
                y: px(oy + (dh - WELCOME_HEIGHT).max(0.0) / 2.0),
            }
        })
        .unwrap_or(Point {
            x: px(260.0),
            y: px(100.0),
        });

    // The OS frame on Windows; the drawn chrome band elsewhere.
    let mut options = platform_chrome::welcome_window_options();
    options.window_bounds = Some(WindowBounds::Windowed(Bounds {
        origin,
        size: welcome_size,
    }));
    options.show = true;
    options.focus = true;
    // The start screen lays out a nav rail beside a two-column body; below this
    // the columns stack and the recent list loses its date column. Without a
    // floor the window can be dragged narrower than anything it can render.
    options.window_min_size = Some(size(px(880.0), px(560.0)));
    options
}
