//! GPUI presentation of an **off-screen** built-in plugin editor.
//!
//! Used only where `OFFSCREEN_HOSTING` is on (Linux). Windows and macOS host
//! the browser as a native CEF child and never touch this surface. For
//! off-screen hosts, accelerated D3D11 shared textures can be copied directly
//! into stable GPUI atlas tiles; software OSR hands the host a BGRA framebuffer
//! instead. This module owns both presentation paths plus input forwarding:
//!
//! - **Out:** reusing stable GPU textures for accelerated frames, or turning a
//!   software frame into a GPUI texture and releasing the previous atlas tile.
//! - **In:** translating GPUI mouse/keyboard events into the logical-pixel,
//!   `VKEY_*`-coded events CEF expects.
//!
//! Everything here runs on the GPUI UI thread, which is also CEF's UI thread.

use std::sync::Arc;

#[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
use gpui::{
    canvas, point, px, size, AnyElement, Bounds, Corners, D3D11ExternalImage, DevicePixels,
    IntoElement, Pixels, Styled,
};
use gpui::{App, Keystroke, Modifiers, MouseButton, RenderImage, Window};
use image::{Frame, ImageBuffer};
use smallvec::SmallVec;

use crate::components::builtin_plugin_editor::{
    self as host, EditorKey, EditorKeyKind, EditorModifiers, EditorMouseButton, ViewId,
};
#[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
use sphere_webview::osr::{OsrAcceleratedFrame, OsrAcceleratedFrameSink, OsrPlane, OsrPopupState};

/// The frame currently uploaded to GPUI, plus the mouse-button state CEF needs
/// echoed back in every event's modifier mask.
#[derive(Default)]
pub(crate) struct OffscreenSurface {
    image: Option<Arc<RenderImage>>,
    /// Surface generation `image` was built from. `0` means "nothing yet".
    generation: u64,
    using_accelerated_frames: bool,
    /// Frames replaced since the last render pass. Their atlas tiles can only
    /// be dropped with a `Window` in hand, which `sync` does not have.
    stale: Vec<Arc<RenderImage>>,
    #[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
    accelerated: Option<Arc<AcceleratedPresentation>>,
    buttons: ButtonState,
}

#[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
struct AcceleratedPresentation {
    view: Arc<D3D11ExternalImage>,
    popup: Arc<D3D11ExternalImage>,
    view_presented: std::sync::atomic::AtomicBool,
    popup_presented: std::sync::atomic::AtomicBool,
    popup_state: std::sync::Mutex<AcceleratedPopupState>,
    /// View frames CEF has published into the GPU texture, and when the newest
    /// one landed (nanoseconds since [`profiling_epoch`]). The compositor reads
    /// both to tell a fresh frame from a reused one and to measure how long a
    /// finished frame waited to be drawn.
    published_generation: std::sync::atomic::AtomicU64,
    published_at_nanos: std::sync::atomic::AtomicU64,
    /// Generation the most recent compositor frame actually drew.
    painted_generation: std::sync::atomic::AtomicU64,
}

#[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
#[derive(Debug, Clone, Copy)]
struct AcceleratedPopupState {
    visible: bool,
    x: i32,
    y: i32,
    scale_factor: f32,
}

#[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
impl Default for AcceleratedPopupState {
    fn default() -> Self {
        Self {
            visible: false,
            x: 0,
            y: 0,
            scale_factor: 1.0,
        }
    }
}

#[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
impl AcceleratedPresentation {
    /// Account for one compositor frame drawing this editor.
    ///
    /// Runs inside the paint closure, so it sees exactly the frames that reach
    /// the screen. A compositor frame that finds the generation unchanged is
    /// *reusing* the previous web texture, which is the intended behaviour on a
    /// display faster than the browser — the number to watch is the redraw wait,
    /// not the reuse count.
    fn record_paint(&self) {
        use gpui::osr_profile::{self, Counter, Stage};
        use std::sync::atomic::Ordering;

        if !osr_profile::enabled() {
            return;
        }
        let published = self.published_generation.load(Ordering::Acquire);
        let painted = self.painted_generation.swap(published, Ordering::AcqRel);
        if published == painted {
            osr_profile::count(Counter::CompositorFramesReusingWebTexture, 1);
            return;
        }
        // Everything between the last painted generation and the newest one was
        // superseded before any frame drew it.
        osr_profile::count(
            Counter::CefFramesDropped,
            published.saturating_sub(painted).saturating_sub(1),
        );
        let published_at = self.published_at_nanos.load(Ordering::Relaxed);
        let now = osr_profile::epoch_nanos();
        osr_profile::record(
            Stage::RedrawWait,
            std::time::Duration::from_nanos(now.saturating_sub(published_at)),
        );
    }
}

#[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
impl OsrAcceleratedFrameSink for AcceleratedPresentation {
    fn present(&self, frame: OsrAcceleratedFrame) -> Result<(), String> {
        use gpui::osr_profile::{self, Counter, Stage};

        // T0. The view plane is the browser's real output cadence; popup paints
        // are event-driven and would make the interval meaningless.
        if frame.plane == OsrPlane::View {
            osr_profile::mark(Stage::CefFrameInterval);
            osr_profile::count(Counter::CefFrames, 1);
            // What changed, against what is actually copied. The copy below is
            // whole-surface regardless, so this is the measurement that says
            // how much of it was wasted.
            osr_profile::count(Counter::DirtyRects, u64::from(frame.dirty_rect_count));
            osr_profile::count(Counter::DirtyPixels, frame.dirty_pixels);
            osr_profile::count(
                Counter::SurfacePixels,
                (frame.width.max(0) as u64) * (frame.height.max(0) as u64),
            );
        }
        let _callback = osr_profile::span(Stage::CefCallback);

        let image = match frame.plane {
            OsrPlane::View => &self.view,
            OsrPlane::Popup => &self.popup,
        };
        image
            .update_from_shared_texture(
                frame.shared_texture_handle,
                point(DevicePixels(frame.source_x), DevicePixels(frame.source_y)),
                size(DevicePixels(frame.width), DevicePixels(frame.height)),
            )
            .map_err(|error| {
                osr_profile::count(Counter::CopyFailures, 1);
                error.to_string()
            })?;
        match frame.plane {
            OsrPlane::View => {
                self.view_presented
                    .store(true, std::sync::atomic::Ordering::Release);
                // Publish the timestamp before the generation, so a compositor
                // that observes the new generation always sees a timestamp that
                // belongs to it or is newer — never an older one.
                if osr_profile::enabled() {
                    self.published_at_nanos.store(
                        osr_profile::epoch_nanos(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                self.published_generation
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
            }
            OsrPlane::Popup => self
                .popup_presented
                .store(true, std::sync::atomic::Ordering::Release),
        }
        Ok(())
    }

    fn set_popup_state(&self, state: OsrPopupState) {
        if let Ok(mut popup) = self.popup_state.lock() {
            *popup = AcceleratedPopupState {
                visible: state.visible,
                x: state.rect.x,
                y: state.rect.y,
                scale_factor: state.scale_factor.max(f32::EPSILON),
            };
        }
    }

    fn is_valid(&self) -> bool {
        let view_valid = !self
            .view_presented
            .load(std::sync::atomic::Ordering::Acquire)
            || self.view.is_available();
        let popup_valid = !self
            .popup_presented
            .load(std::sync::atomic::Ordering::Acquire)
            || self.popup.is_available();
        view_valid && popup_valid
    }
}

#[derive(Default, Clone, Copy)]
struct ButtonState {
    left: bool,
    middle: bool,
    right: bool,
}

impl OffscreenSurface {
    /// Pull the latest painted frame for `view_id`. Returns `true` when a new
    /// frame was taken and the window therefore needs to repaint.
    pub(crate) fn sync(&mut self, view_id: ViewId) -> bool {
        let using_accelerated_frames = host::view_uses_accelerated_osr(view_id);
        if using_accelerated_frames != self.using_accelerated_frames {
            self.using_accelerated_frames = using_accelerated_frames;
            self.generation = 0;
        }
        let generation = host::view_frame_generation(view_id);
        if generation == 0 || generation == self.generation {
            return false;
        }
        #[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
        if using_accelerated_frames {
            // On accelerated Windows OSR the callback already copied the frame
            // into a stable GPUI D3D11 atlas tile. Only the generation changes;
            // no CPU image or per-frame allocation is needed here.
            self.generation = generation;
            return true;
        }
        let Some(Some(image)) = host::with_view_frame(view_id, |bgra, width, height| {
            // GPUI's `RenderImage` is BGRA with premultiplied alpha, which is
            // exactly CEF's `OnPaint` layout — no channel swap needed.
            let buffer = ImageBuffer::from_raw(width as u32, height as u32, bgra.to_vec())?;
            Some(Arc::new(RenderImage::new(SmallVec::from_elem(
                Frame::new(buffer),
                1,
            ))))
        }) else {
            return false;
        };

        self.generation = generation;
        if let Some(previous) = self.image.replace(image) {
            self.stale.push(previous);
        }
        true
    }

    /// The texture to draw, if a frame has arrived.
    pub(crate) fn image(&self) -> Option<Arc<RenderImage>> {
        self.image.clone()
    }

    /// Prepare the GPU-to-GPU CEF presentation path for this window. Returning
    /// `None` selects the existing software `OnPaint` surface instead.
    #[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
    pub(crate) fn accelerated_sink(
        &mut self,
        window: &Window,
    ) -> Option<host::AcceleratedFrameSink> {
        if std::env::var_os("FUTUREBOARD_CEF_SOFTWARE_OSR").is_some() {
            return None;
        }
        let presentation = self.accelerated.get_or_insert_with(|| {
            Arc::new(AcceleratedPresentation {
                view: window.create_d3d11_external_image(),
                popup: window.create_d3d11_external_image(),
                view_presented: std::sync::atomic::AtomicBool::new(false),
                popup_presented: std::sync::atomic::AtomicBool::new(false),
                popup_state: std::sync::Mutex::new(AcceleratedPopupState::default()),
                published_generation: std::sync::atomic::AtomicU64::new(0),
                published_at_nanos: std::sync::atomic::AtomicU64::new(0),
                painted_generation: std::sync::atomic::AtomicU64::new(0),
            })
        });
        Some(presentation.clone())
    }

    #[cfg(not(all(feature = "builtin-plugin-editor", target_os = "windows")))]
    pub(crate) fn accelerated_sink(
        &mut self,
        _window: &Window,
    ) -> Option<host::AcceleratedFrameSink> {
        None
    }

    /// Draw the two stable accelerated planes. Popups remain separate so hiding
    /// one immediately reveals the unchanged view texture below it.
    #[cfg(all(feature = "builtin-plugin-editor", target_os = "windows"))]
    pub(crate) fn accelerated_element(&self) -> Option<AnyElement> {
        let presentation = self.accelerated.clone()?;
        Some(
            canvas(
                |bounds, _, _| bounds,
                move |bounds: Bounds<Pixels>, _, window, _| {
                    presentation.record_paint();
                    let _ = window.paint_d3d11_external_image(
                        bounds,
                        Corners::default(),
                        presentation.view.clone(),
                    );
                    let popup = presentation
                        .popup_state
                        .lock()
                        .map(|popup| *popup)
                        .unwrap_or_default();
                    let popup_size = presentation.popup.size();
                    if popup.visible && popup_size.width.0 > 0 && popup_size.height.0 > 0 {
                        let popup_bounds = Bounds {
                            origin: point(
                                bounds.origin.x + px(popup.x as f32),
                                bounds.origin.y + px(popup.y as f32),
                            ),
                            size: size(
                                px(popup_size.width.0 as f32 / popup.scale_factor),
                                px(popup_size.height.0 as f32 / popup.scale_factor),
                            ),
                        };
                        let _ = window.paint_d3d11_external_image(
                            popup_bounds,
                            Corners::default(),
                            presentation.popup.clone(),
                        );
                    }
                },
            )
            .absolute()
            .size_full()
            .into_any_element(),
        )
    }

    #[cfg(not(all(feature = "builtin-plugin-editor", target_os = "windows")))]
    pub(crate) fn accelerated_element(&self) -> Option<gpui::AnyElement> {
        None
    }

    /// Drop the atlas tiles of every superseded frame. Must be called from a
    /// render pass; without it each uploaded frame would leak a texture.
    pub(crate) fn release_stale(&mut self, window: &mut Window, cx: &mut App) {
        for image in self.stale.drain(..) {
            cx.drop_image(image, Some(window));
        }
    }

    pub(crate) fn set_button(&mut self, button: EditorMouseButton, pressed: bool) {
        match button {
            EditorMouseButton::Left => self.buttons.left = pressed,
            EditorMouseButton::Middle => self.buttons.middle = pressed,
            EditorMouseButton::Right => self.buttons.right = pressed,
        }
    }

    /// Whether the page currently owns a pointer gesture. While a button is
    /// held the browser must keep receiving moves — and must never be told the
    /// pointer left — or Blink cancels the captured pointer mid-drag.
    pub(crate) fn any_button_held(&self) -> bool {
        self.buttons.left || self.buttons.middle || self.buttons.right
    }

    /// Forget every held button, returning whether any were held.
    ///
    /// Used on native capture loss. Deliberately does **not** synthesize the
    /// releases the page never received: CEF's `SendCaptureLostEvent` already
    /// makes Blink end the captured gesture, and adding a mouse-up on top would
    /// make a knob take the release twice — once as a cancel, once as a commit
    /// at whatever position the pointer had drifted to.
    pub(crate) fn clear_buttons(&mut self) -> bool {
        let held = self.any_button_held();
        self.buttons = ButtonState::default();
        held
    }

    /// Modifier mask for an outgoing event: keyboard modifiers from GPUI plus
    /// the buttons this surface believes are held.
    pub(crate) fn modifiers(&self, modifiers: Modifiers) -> EditorModifiers {
        EditorModifiers {
            shift: modifiers.shift,
            control: modifiers.control,
            alt: modifiers.alt,
            command: modifiers.platform,
            left_button: self.buttons.left,
            middle_button: self.buttons.middle,
            right_button: self.buttons.right,
        }
    }
}

/// GPUI button → CEF button. Back/forward have no CEF equivalent and are
/// dropped rather than mapped onto a real button.
pub(crate) fn editor_mouse_button(button: MouseButton) -> Option<EditorMouseButton> {
    match button {
        MouseButton::Left => Some(EditorMouseButton::Left),
        MouseButton::Middle => Some(EditorMouseButton::Middle),
        MouseButton::Right => Some(EditorMouseButton::Right),
        _ => None,
    }
}

/// Chromium `VKEY_*` code for a GPUI key name, or `None` for a key CEF has no
/// virtual code for (the character still travels as a `Char` event).
pub(crate) fn windows_key_code(key: &str) -> Option<i32> {
    let code = match key {
        "backspace" => 0x08,
        "tab" => 0x09,
        "enter" => 0x0D,
        "shift" => 0x10,
        "control" => 0x11,
        "alt" => 0x12,
        "capslock" => 0x14,
        "escape" => 0x1B,
        // Linux/XKB and some IMs emit bare space as `" "` / `"Spacebar"`.
        // Without this mapping CEF never sees a RAWKEYDOWN Space for transport.
        "space" | " " | "spacebar" | "Space" | "Spacebar" | "kp-space" => 0x20,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "insert" => 0x2D,
        "delete" => 0x2E,
        ";" => 0xBA,
        "=" => 0xBB,
        "," => 0xBC,
        "-" => 0xBD,
        "." => 0xBE,
        "/" => 0xBF,
        "`" => 0xC0,
        "[" => 0xDB,
        "\\" => 0xDC,
        "]" => 0xDD,
        "'" => 0xDE,
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                // Single ASCII alphanumerics share their uppercase code point.
                (Some(c), None) if c.is_ascii_digit() => c as i32,
                (Some(c), None) if c.is_ascii_alphabetic() => c.to_ascii_uppercase() as i32,
                _ => {
                    // f1..f24
                    let function = key
                        .strip_prefix('f')
                        .and_then(|n| n.parse::<i32>().ok())
                        .filter(|n| (1..=24).contains(n))?;
                    0x70 + function - 1
                }
            }
        }
    };
    Some(code)
}

/// The key-down/key-up event for `keystroke`, if it maps to a virtual code.
pub(crate) fn editor_key(
    keystroke: &Keystroke,
    kind: EditorKeyKind,
    modifiers: EditorModifiers,
) -> Option<EditorKey> {
    Some(EditorKey {
        kind,
        windows_key_code: windows_key_code(&keystroke.key)?,
        character: 0,
        modifiers,
    })
}

/// The `Char` events for the text `keystroke` would type, one per UTF-16 code
/// unit. Empty when the keystroke produced no text (a bare modifier, an
/// accelerator, a navigation key).
pub(crate) fn editor_char_keys(
    keystroke: &Keystroke,
    modifiers: EditorModifiers,
) -> Vec<EditorKey> {
    // A key_char alongside control/platform is an accelerator, not typed text;
    // forwarding it would insert a character *and* run the shortcut.
    if modifiers.control || modifiers.command {
        return Vec::new();
    }
    let Some(text) = keystroke.key_char.as_deref() else {
        return Vec::new();
    };
    text.encode_utf16()
        .map(|unit| EditorKey {
            kind: EditorKeyKind::Char,
            windows_key_code: unit as i32,
            character: unit,
            modifiers,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: Modifiers::default(),
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    #[test]
    fn named_keys_map_to_their_chromium_virtual_codes() {
        assert_eq!(windows_key_code("enter"), Some(0x0D));
        assert_eq!(windows_key_code("backspace"), Some(0x08));
        assert_eq!(windows_key_code("left"), Some(0x25));
        assert_eq!(windows_key_code("delete"), Some(0x2E));
        assert_eq!(windows_key_code("space"), Some(0x20));
        assert_eq!(windows_key_code(" "), Some(0x20));
        assert_eq!(windows_key_code("Spacebar"), Some(0x20));
    }

    #[test]
    fn ascii_keys_use_their_uppercase_code_point() {
        assert_eq!(windows_key_code("a"), Some('A' as i32));
        assert_eq!(windows_key_code("z"), Some('Z' as i32));
        assert_eq!(windows_key_code("7"), Some('7' as i32));
    }

    #[test]
    fn function_keys_are_offsets_from_vkey_f1() {
        assert_eq!(windows_key_code("f1"), Some(0x70));
        assert_eq!(windows_key_code("f12"), Some(0x7B));
        assert_eq!(windows_key_code("f99"), None);
    }

    #[test]
    fn unmapped_keys_produce_no_key_event() {
        assert_eq!(windows_key_code("brightnessup"), None);
        assert!(editor_key(
            &keystroke("brightnessup", None),
            EditorKeyKind::Down,
            EditorModifiers::default()
        )
        .is_none());
    }

    #[test]
    fn typed_text_becomes_one_char_event_per_utf16_unit() {
        let keys = editor_char_keys(&keystroke("a", Some("a")), EditorModifiers::default());
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].character, 'a' as u16);
        assert_eq!(keys[0].kind, EditorKeyKind::Char);

        // Outside the BMP: two surrogate halves, both forwarded.
        let keys = editor_char_keys(&keystroke("g", Some("𝄞")), EditorModifiers::default());
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn accelerators_do_not_type_a_character() {
        let modifiers = EditorModifiers {
            control: true,
            ..Default::default()
        };
        assert!(editor_char_keys(&keystroke("a", Some("a")), modifiers).is_empty());
    }

    #[test]
    fn a_keystroke_without_text_types_nothing() {
        assert!(editor_char_keys(&keystroke("left", None), EditorModifiers::default()).is_empty());
    }

    #[test]
    fn button_state_is_echoed_into_the_modifier_mask() {
        let mut surface = OffscreenSurface::default();
        assert!(!surface.modifiers(Modifiers::default()).left_button);
        surface.set_button(EditorMouseButton::Left, true);
        assert!(surface.modifiers(Modifiers::default()).left_button);
        surface.set_button(EditorMouseButton::Left, false);
        assert!(!surface.modifiers(Modifiers::default()).left_button);
    }

    /// Capture loss has to leave no button believed held: a stuck flag makes
    /// every later move carry a phantom button bit and permanently suppresses
    /// the pointer-leave notification, so the page never clears hover again.
    #[test]
    fn clearing_buttons_reports_whether_a_gesture_was_interrupted() {
        let mut surface = OffscreenSurface::default();
        assert!(
            !surface.clear_buttons(),
            "nothing held means there is no gesture to end"
        );

        surface.set_button(EditorMouseButton::Left, true);
        surface.set_button(EditorMouseButton::Right, true);
        assert!(surface.clear_buttons());
        assert!(!surface.any_button_held());

        let modifiers = surface.modifiers(Modifiers::default());
        assert!(!modifiers.left_button && !modifiers.middle_button && !modifiers.right_button);
        assert!(
            !surface.clear_buttons(),
            "a repeated deactivation must not produce a second CEF event"
        );
    }

    #[test]
    fn gpui_modifiers_map_onto_cef_flags() {
        let surface = OffscreenSurface::default();
        let modifiers = surface.modifiers(Modifiers {
            shift: true,
            platform: true,
            ..Default::default()
        });
        assert!(modifiers.shift && modifiers.command);
        assert!(!modifiers.control && !modifiers.alt);
    }

    #[test]
    fn navigation_buttons_are_not_forwarded() {
        assert_eq!(
            editor_mouse_button(MouseButton::Left),
            Some(EditorMouseButton::Left)
        );
        assert_eq!(
            editor_mouse_button(MouseButton::Navigate(gpui::NavigationDirection::Back)),
            None
        );
    }
}
