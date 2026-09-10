//! The bound ARA plug-in's own view, hosted inside the docked Editor panel.
//!
//! # How it works
//!
//! A VST3 editor is a real native child window, not a texture, so it cannot be
//! painted by GPUI. Instead the panel reserves a rectangle and a native child
//! window is parked over it:
//!
//! ```txt
//! studio window (HWND)
//!   └── ContentChildHwnd  ← positioned to the measured panel rect
//!         └── plug-in IPlugView
//! ```
//!
//! GPUI only learns an element's laid-out rect during prepaint, one frame after
//! the render that produced it, so the measured bounds are stashed in a cell and
//! applied on the next frame — the same shape `AudioEditorHost` uses for its
//! viewport width. Nothing else in this repo drove a native child from a
//! *measured element* rect before; every other native host derives its rect from
//! the whole window.
//!
//! # Ownership
//!
//! This view never creates a plug-in instance. It borrows the one the ARA
//! session already bound and the engine already renders, so the editor always
//! edits the audio you hear. When the panel stops showing it — tab switched,
//! dock hidden, another clip selected, or popped out to its own window — the
//! view is detached and the child window destroyed, because a stray child would
//! otherwise float over whatever the panel shows next.
//!
//! # Platforms
//!
//! Windows embeds. macOS has no native-child embedding anywhere in this crate
//! (`ContentChildHwnd` is a stub there), so the panel explains that and offers
//! the pop-out window, which macOS does support.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, px, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, Styled, Window,
};

use crate::components::plugin_content_host::{ContentChildHwnd, ContentRect};
use crate::layout::ara_ops::AraSessionKey;
use crate::layout::StudioLayout;
use crate::theme::Colors;

/// Smallest region worth handing a plug-in. Below this the panel is effectively
/// collapsed and resizing the view would only thrash it.
const MIN_EMBED_PX: i32 = 24;

/// Where the embed attempt got to.
///
/// `PluginEditorWindow` states its own rule as "a blank panel never appears
/// unless we are actually `Attached` with a live native child", and the same has
/// to hold here: the attached region is deliberately empty because the plug-in
/// covers it, so every other outcome must say what happened instead.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AraPanelStatus {
    /// Waiting for the first prepaint to measure the panel rect.
    Measuring,
    /// A live native child is parked over the panel.
    Attached,
    /// Nothing is embedded, and this is why.
    Failed(String),
}

pub struct AraEditorHost {
    owner: Entity<StudioLayout>,
    /// Panel rect measured during the previous frame's prepaint, in logical
    /// pixels relative to the window.
    measured: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Native child currently hosting the plug-in view.
    content: Option<ContentChildHwnd>,
    /// Last rect pushed to the child, so an unchanged layout costs nothing.
    last_rect: Option<ContentRect>,
    /// What the panel should be showing right now.
    status: AraPanelStatus,
    /// Studio window handle captured during render, for the deferred attach.
    parent_hwnd: Option<u64>,
    /// Session the panel wants shown, as of the last render.
    pending: Option<AraSessionKey>,
    /// Rect the panel wants the view at, as of the last render.
    pending_rect: Option<ContentRect>,
    /// Whether a deferred sync is already queued.
    tick_scheduled: bool,
    /// The session this panel has already asked for a window for.
    ///
    /// Only used where the dock cannot embed. Once per session, not once per
    /// frame and not once per state change: `render` runs constantly, and the
    /// studio opens the window several frames later. Keyed rather than a plain
    /// flag so that closing the window leaves it closed — reopening it on the
    /// next frame would make it unclosable — while binding a different clip
    /// still gets its own editor.
    own_window_requested: Option<AraSessionKey>,
    /// Last line written by [`Self::trace`], so a per-frame state is reported
    /// once rather than every frame.
    traced: Option<String>,
    /// Session whose view is embedded, and the exact instance it belongs to.
    ///
    /// The processor handle is kept here rather than re-read on teardown: detach
    /// has to work even when the owner is mid-update or the session has already
    /// been closed, and releasing the view is not optional.
    attached: Option<(AraSessionKey, DirectAudio::Vst3RuntimeProcessor)>,
    /// A size the plug-in asked for, taken off the bridge and waiting for the
    /// deferred tick to answer it.
    ///
    /// The panel cannot grow for a plug-in — its size is the dock's — so the
    /// answer is always "here is what you actually have". Recording it rather
    /// than replying from `render` keeps every plug-in call on the deferred
    /// tick, which is the rule the whole native side of this view follows.
    pending_view_resize: Option<(i32, i32)>,
    focus: FocusHandle,
}

impl AraEditorHost {
    pub fn new(owner: Entity<StudioLayout>, cx: &mut Context<Self>) -> Self {
        Self {
            owner,
            measured: Rc::new(Cell::new(None)),
            content: None,
            last_rect: None,
            parent_hwnd: None,
            pending: None,
            pending_rect: None,
            tick_scheduled: false,
            own_window_requested: None,
            traced: None,
            status: AraPanelStatus::Measuring,
            attached: None,
            pending_view_resize: None,
            focus: cx.focus_handle(),
        }
    }

    /// Whether a plug-in view is currently parked over the panel.
    pub fn is_attached(&self) -> bool {
        self.attached.is_some()
    }

    /// The session whose view is embedded, if any.
    pub fn attached_key(&self) -> Option<&AraSessionKey> {
        self.attached.as_ref().map(|(key, _)| key)
    }

    /// Tears the embedded view down.
    ///
    /// Order matters: the plug-in releases its view first, then the child window
    /// is destroyed — destroying a parent out from under a live `IPlugView` is
    /// exactly what `DESIGN.md` forbids.
    pub fn detach(&mut self) {
        if let Some((_, processor)) = self.attached.take() {
            processor.view_detach();
        }
        self.content = None;
        self.last_rect = None;
        self.pending_view_resize = None;
        self.status = AraPanelStatus::Measuring;
    }

    /// Asks for the view to come down, without touching it here.
    ///
    /// Tearing down goes through the same deferred tick as attaching:
    /// `embed_detach` destroys the plug-in's window, and `DestroyWindow`
    /// dispatches `WM_DESTROY` synchronously, which can re-enter GPUI exactly
    /// the way the attach path does.
    pub fn request_detach(&mut self, cx: &mut Context<Self>) {
        self.pending = None;
        self.pending_rect = None;
        if self.attached.is_some() {
            self.schedule_sync(cx);
        } else {
            self.status = AraPanelStatus::Measuring;
        }
    }

    /// Reports a state change on the docked-editor path.
    ///
    /// `render` runs every frame, so the same line is written only when it
    /// actually changes; `DESIGN.md` requires high-rate logs to be gated, and
    /// this one is additionally deduplicated.
    /// Takes a closure so nothing is formatted, and nothing is allocated, on the
    /// overwhelmingly common path where the trace is off. `render` runs every
    /// frame; building a string there to throw it away is work the panel does
    /// forever and reads never.
    fn trace(&mut self, line: impl FnOnce() -> String) {
        if !view_debug() {
            return;
        }
        let line = line();
        if self.traced.as_deref() == Some(line.as_str()) {
            return;
        }
        eprintln!("[ara-panel] {line}");
        self.traced = Some(line);
    }

    /// Reports a one-off event on the docked-editor path (not per frame).
    fn trace_event(line: &str) {
        if view_debug() {
            eprintln!("[ara-panel] {line}");
        }
    }

    /// Records why nothing is embedded, without spamming a repeat.
    fn fail(&mut self, reason: impl Into<String>) {
        let reason = AraPanelStatus::Failed(reason.into());
        if self.status != reason {
            self.status = reason;
        }
    }

    /// Converts the measured logical rect into the physical, parent-relative
    /// rect the native child and the plug-in both expect.
    fn embed_rect(bounds: Bounds<Pixels>, scale: f32) -> Option<ContentRect> {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let rect = ContentRect {
            x: (f32::from(bounds.origin.x) * scale).round() as i32,
            y: (f32::from(bounds.origin.y) * scale).round() as i32,
            width: (f32::from(bounds.size.width) * scale).round() as i32,
            height: (f32::from(bounds.size.height) * scale).round() as i32,
        };
        (rect.width >= MIN_EMBED_PX && rect.height >= MIN_EMBED_PX).then_some(rect)
    }

    /// Queues the native work for after the current draw.
    ///
    /// Everything that touches the plug-in's window has to happen outside
    /// `render`. `embed_editor` pumps the Windows message loop while it waits
    /// for the view to settle, which re-enters GPUI and runs whatever foreground
    /// task is queued; doing that while the draw holds `App`'s `RefCell` borrow
    /// panics with "RefCell already borrowed".
    fn schedule_sync(&mut self, cx: &mut Context<Self>) {
        if self.tick_scheduled {
            return;
        }
        self.tick_scheduled = true;
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            executor.timer(Duration::from_millis(16)).await;
            let _ = this.update(cx, |this, cx| {
                this.tick_scheduled = false;
                this.perform_sync(cx);
            });
        })
        .detach();
    }

    /// Whether the native side is out of step with what the panel wants.
    fn sync_needed(&self, key: &AraSessionKey, rect: ContentRect) -> bool {
        match &self.attached {
            None => true,
            Some((current, _)) => current != key || self.last_rect != Some(rect),
        }
    }

    /// Creates, moves, or tears down the plug-in's view. Never called from
    /// `render` — see [`Self::schedule_sync`].
    fn perform_sync(&mut self, cx: &mut Context<Self>) {
        Self::trace_event("perform_sync enter");
        let Some(key) = self.pending.clone() else {
            Self::trace_event("perform_sync: no target -> detach");
            self.detach();
            cx.notify();
            return;
        };
        // A different session means a different plug-in view; the old one has to
        // go before the new one is parked in the same place.
        if self
            .attached
            .as_ref()
            .is_some_and(|(current, _)| current != &key)
        {
            self.detach();
        }
        let Some(rect) = self.pending_rect else {
            Self::trace_event("perform_sync: no measured rect yet");
            return;
        };
        let Some(processor) = self.owner.read(cx).ara_processor(&key) else {
            self.detach();
            let detail = self
                .owner
                .read(cx)
                .ara_last_error()
                .unwrap_or_else(|| "no ARA session is running for this track".to_string());
            self.fail(detail);
            cx.notify();
            return;
        };

        if self.attached.is_none() {
            let Some(parent) = self.parent_hwnd else {
                self.fail("could not reach the studio window to host the editor");
                cx.notify();
                return;
            };
            Self::trace_event(&format!(
                "perform_sync: creating child parent=0x{parent:x} rect=({},{},{}x{})",
                rect.x, rect.y, rect.width, rect.height
            ));
            let Some(content) = ContentChildHwnd::create(parent, rect) else {
                self.fail("could not create the editor surface");
                cx.notify();
                return;
            };
            // GPUI owns the window from here on. The bridge only creates the
            // plug-in's view and attaches it to the child above — it does not
            // create a shell, a titlebar, or a window procedure of its own, so
            // this panel is the single owner of the editor's geometry.
            let Some(preferred) = processor.view_attach(content.hwnd(), (rect.width, rect.height))
            else {
                self.fail("the plug-in did not open its editor");
                cx.notify();
                return;
            };
            let granted = Self::grant_size(&processor, rect);
            Self::trace_event(&format!(
                "perform_sync: view attached preferred={}x{} granted={}x{}",
                preferred.0, preferred.1, granted.0, granted.1
            ));
            self.content = Some(content);
            self.last_rect = Some(rect);
            self.pending_view_resize = None;
            self.attached = Some((key, processor));
            self.status = AraPanelStatus::Attached;
            cx.notify();
            return;
        }

        if self.last_rect != Some(rect) {
            if let Some(content) = self.content.as_ref() {
                content.set_bounds(rect);
            }
            Self::grant_size(&processor, rect);
            self.last_rect = Some(rect);
            self.pending_view_resize = None;
        } else if let Some((request_w, request_h)) = self.pending_view_resize.take() {
            // The plug-in asked to change size. A docked panel has no size of
            // its own to give, so it is answered with the region it already
            // has: the view relays out inside it instead of the window moving.
            let granted = Self::grant_size(&processor, rect);
            Self::trace_event(&format!(
                "perform_sync: resize request {request_w}x{request_h} answered with {}x{}",
                granted.0, granted.1
            ));
        }
        if self.status != AraPanelStatus::Attached {
            self.status = AraPanelStatus::Attached;
            cx.notify();
        }
    }

    /// Tells the view the size the panel is actually giving it.
    ///
    /// Run through the VST3 size contract first: a resizable view takes the
    /// region as-is, a fixed one keeps its own size and is simply clipped by
    /// the panel rather than being told a size it cannot honour.
    fn grant_size(processor: &DirectAudio::Vst3RuntimeProcessor, rect: ContentRect) -> (i32, i32) {
        let (width, height) = processor.view_constrain(rect.width, rect.height);
        processor.view_set_size(width, height);
        (width, height)
    }
}

impl AraEditorHost {
    /// Ask the studio to put this editor in its own window.
    ///
    /// Only where the dock cannot embed. Opening a window reaches the platform
    /// and dispatches messages that re-enter GPUI, so it is never done from a
    /// render — this only sets the studio's flag, on a later turn of the loop,
    /// and `pop_out_ara_editor` does the rest on its own schedule.
    ///
    /// Asked once per session: `render` runs every frame, and the studio opens
    /// the window several frames later. Asking again once the window has been
    /// closed would make it impossible to close.
    fn request_own_window(&mut self, key: &AraSessionKey, cx: &mut Context<Self>) {
        if self.own_window_requested.as_ref() == Some(key) {
            return;
        }
        self.own_window_requested = Some(key.clone());
        let owner = self.owner.clone();
        cx.spawn(async move |_this, cx| {
            let _ = owner.update(cx, |layout, cx| {
                if !layout.ara_editor_is_popped_out() {
                    layout.pop_out_ara_editor(cx);
                }
            });
        })
        .detach();
    }
}

impl Focusable for AraEditorHost {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AraEditorHost {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _scope = crate::perf::PerfScope::enter("BottomPanelAraEditor");

        if self.owner.read(cx).ara_editor_is_popped_out() {
            self.trace(|| "render: popped out".to_string());
            self.request_detach(cx);
            return unavailable_panel(if embedding_supported() {
                "This editor is open in its own window — use Dock to bring it back."
            } else {
                "This editor is open in its own window."
            })
            .into_any_element();
        }
        let target = self.owner.read(cx).ara_panel_target(cx);
        let Some(key) = target else {
            self.trace(|| "render: no ARA target for the selection".to_string());
            self.request_detach(cx);
            // Nothing bound here, so the next thing that is gets to ask for a
            // window of its own.
            self.own_window_requested = None;
            return unavailable_panel("This clip is not being edited by an ARA plug-in.")
                .into_any_element();
        };

        if !embedding_supported() {
            // No native-child embedding here, so the dock has nothing it can
            // show. The editor goes to the same window every other plug-in's
            // does, with the same chrome strip above it — asking the user to
            // find "Pop Out" first was a dead end dressed as a choice.
            self.request_detach(cx);
            self.request_own_window(&key, cx);
            return unavailable_panel(
                "This plug-in's editor opens in its own window on this platform. \
                 Closing that window closes the editor.",
            )
            .into_any_element();
        }

        // Record the target and let the deferred tick do the native work.
        self.parent_hwnd = native_window_handle(window);
        self.pending = Some(key.clone());
        self.pending_rect = self
            .measured
            .get()
            .and_then(|bounds| Self::embed_rect(bounds, window.scale_factor()));
        let pending_rect = self.pending_rect;
        let attached = self.attached.is_some();
        let status = self.status.clone();
        let plugin_id = key.plugin_id.clone();
        self.trace(move || {
            format!("render: target={plugin_id} rect={pending_rect:?} attached={attached} status={status:?}")
        });
        // A plug-in can ask to resize at any moment. Taking the request here is
        // one atomic exchange on the bridge — no plug-in call, no window work —
        // and the answer goes out on the deferred tick with everything else.
        if self.pending_view_resize.is_none() {
            if let Some((_, processor)) = self.attached.as_ref() {
                self.pending_view_resize = processor.view_take_resize_request();
            }
        }
        match self.pending_rect {
            Some(rect) if self.sync_needed(&key, rect) => self.schedule_sync(cx),
            Some(_) if self.pending_view_resize.is_some() => self.schedule_sync(cx),
            // A collapsed dock is a state, not a failure; the view comes back
            // when the panel is dragged open again.
            None if self.attached.is_some() => self.schedule_sync(cx),
            _ => {}
        }

        let plugin_name = self
            .owner
            .read(cx)
            .ara_plugin_name(&key.plugin_id)
            .unwrap_or_else(|| key.plugin_id.clone());
        // Only the attached region is deliberately blank; every other state says
        // what it is doing, so an empty Editor tab always means a live view.
        let overlay = match &self.status {
            AraPanelStatus::Attached => None,
            AraPanelStatus::Measuring => Some(format!("Opening {plugin_name}…")),
            AraPanelStatus::Failed(reason) => {
                Some(format!("{plugin_name} could not be shown here — {reason}."))
            }
        };

        let measured = Rc::clone(&self.measured);
        let attached = self.status == AraPanelStatus::Attached;
        let mut root = div()
            .key_context("AraEditor")
            .track_focus(&self.focus)
            .size_full();
        if !attached {
            // Opaque only until the native child covers the region. Once it is
            // parked, painting here is pointless rather than harmful: the app
            // boots with `GPUI_DISABLE_DIRECT_COMPOSITION=1`, so gpui gives its
            // windows `WS_CLIPCHILDREN` (`gpui_windows/src/window.rs:502`) and
            // the child's rectangle is removed from the window's visible region
            // — GPUI cannot paint over a plug-in's own HWND, and it cannot hide
            // one either. See `plugin_editor_chrome::PresetMenuWindow` for the
            // consequence: chrome that has to appear over a plug-in's surface
            // has to be a window, not an element.
            root = root.bg(Colors::surface_base());
        }
        root.on_children_prepainted(move |bounds, _window, cx| {
            let Some(bounds) = bounds.first().copied() else {
                return;
            };
            let changed = measured.get().is_none_or(|previous| {
                (f32::from(previous.origin.x) - f32::from(bounds.origin.x)).abs() > 0.5
                    || (f32::from(previous.origin.y) - f32::from(bounds.origin.y)).abs() > 0.5
                    || (f32::from(previous.size.width) - f32::from(bounds.size.width)).abs() > 0.5
                    || (f32::from(previous.size.height) - f32::from(bounds.size.height)).abs() > 0.5
            });
            if changed {
                measured.set(Some(bounds));
                // Only on a real move: an unconditional refresh here would
                // schedule a frame from inside every frame.
                cx.refresh_windows();
            }
        })
        // One measured child that fills the panel; its rect is the region the
        // plug-in view is parked over.
        .child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .children(overlay.map(|text| {
                    div()
                        .text_size(px(11.0))
                        .text_color(Colors::text_muted())
                        .child(text)
                })),
        )
        .into_any_element()
    }
}

/// The panel body shown when no plug-in view can be embedded.
fn unavailable_panel(message: &str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .size_full()
        .bg(Colors::surface_base())
        .text_size(px(11.0))
        .text_color(Colors::text_muted())
        .child(message.to_string())
}

/// Whether the docked-editor trace is enabled.
///
/// Resolved once: `var_os` takes the process-wide environment lock and
/// allocates, and this is asked on every frame the panel renders.
fn view_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some())
}

/// Whether a plug-in view can be parked inside a GPUI panel on this platform.
#[cfg(target_os = "windows")]
fn embedding_supported() -> bool {
    true
}

#[cfg(not(target_os = "windows"))]
fn embedding_supported() -> bool {
    false
}

#[cfg(target_os = "windows")]
fn native_window_handle(window: &Window) -> Option<u64> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // `Window::window_handle()` is GPUI's own inherent method; the raw handle is
    // the same-named trait method, so it has to be called qualified.
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(w) => Some(w.hwnd.get() as u64),
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
fn native_window_handle(_window: &Window) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        }
    }

    #[test]
    fn logical_bounds_convert_to_physical_pixels() {
        let rect = AraEditorHost::embed_rect(bounds(10.0, 20.0, 400.0, 200.0), 1.5).unwrap();
        assert_eq!(rect.x, 15);
        assert_eq!(rect.y, 30);
        assert_eq!(rect.width, 600);
        assert_eq!(rect.height, 300);
    }

    #[test]
    fn a_collapsed_panel_yields_no_rect() {
        // Dragging the dock shut must not hand the plug-in a 0-height view.
        assert!(AraEditorHost::embed_rect(bounds(0.0, 0.0, 400.0, 4.0), 1.0).is_none());
        assert!(AraEditorHost::embed_rect(bounds(0.0, 0.0, 4.0, 400.0), 1.0).is_none());
    }

    #[test]
    fn a_nonsense_scale_factor_falls_back_to_one() {
        // `scale_factor()` has returned 0 during window teardown; a 0 rect would
        // be pushed straight into SetWindowPos.
        let rect = AraEditorHost::embed_rect(bounds(0.0, 0.0, 400.0, 200.0), 0.0).unwrap();
        assert_eq!(rect.width, 400);
        assert_eq!(rect.height, 200);
    }
}
