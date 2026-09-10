//! Main-app-owned **content child HWND** for separated-process plugin editor
//! hosting (`FUTUREBOARD_PLUGIN_EDITOR_OWNERSHIP=host_process`).
//!
//! Spec Part 2: when the editor lives in `FutureboardPluginHostX64.exe`, the
//! *window* still belongs to the GPUI main app. The GPUI plugin-editor window
//! supplies its top HWND; this module creates a real `WS_CHILD` content HWND
//! under it and hands the content HWND's handle to the host process over IPC.
//! The host attaches the VST3 `IPlugView` to that handle from its own COM STA
//! thread (cross-process embedding).
//!
//! VST3 editor hosting follows public.sdk/samples/vst-hosting/editorhost
//! lifecycle: the difference here is only *which process* owns the window
//! (main app) versus the view (host process).
//!
//! Hard requirements enforced here:
//! - `content_hwnd != top_hwnd` (a dedicated child, never the top window).
//! - content child styles: `WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS`.
//! - the child's parent is the supplied top HWND.
//!
//! On non-Windows targets every entry point is a no-op stub returning `None`,
//! and nothing asks: macOS and Linux use the host-owned-window backend, where
//! the plug-in's view lives in a top-level window the host process created and
//! there is no content child in this process at all. See
//! [`crate::components::plugin_editor_backend::EditorBackendKind`].

/// Whether this platform can embed a plug-in's own native view inside the
/// app's window.
///
/// Windows only, and the design above says why: the main app creates a
/// `WS_CHILD` content HWND and hands the handle to the host process, which
/// attaches the VST3 `IPlugView` to it from its own thread. Reparenting a
/// window across process boundaries is a Win32 facility. macOS has no public
/// equivalent for `NSView`, so its editor is not this file's stub growing a
/// body — it is the host-owned backend, where the host process opens the
/// `NSWindow` itself and this file is never consulted.
///
/// Callers consult this to tell "not implemented on this platform" from "the
/// window was not ready this frame". The two look identical from
/// [`ContentChildHwnd::create`] — both are `None` — and reporting the first as
/// the second is what put a Retry button on a dialog that could never succeed.
///
/// Which backend each platform uses, and what it is therefore waiting for,
/// live in [`crate::components::plugin_editor_backend`] — this constant is the
/// content-child half of that answer and stays here, beside the stub it
/// describes.
pub const NATIVE_VIEW_EMBEDDING_SUPPORTED: bool = cfg!(target_os = "windows");

/// Physical-pixel rect (relative to the parent client area) for the content
/// child window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContentRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// What kind of view a content host holds, which decides who owns the keys that
/// land inside it.
///
/// A native plug-in view is opaque: nothing in this process can tell a note
/// editor from a search box inside it, so the transport key is claimed at the
/// window level and only a Win32 caret class can veto it. A web view is not
/// opaque — CEF knows whether a DOM text field has focus and says so on every
/// key — so the transport key is left alone here and claimed there instead.
/// Claiming it at both would take Space away from the editor's own text
/// fields, which is exactly what the caret veto exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentHostKind {
    /// A plug-in's own native view (VST3 `IPlugView`, ARA editor).
    NativeView,
    /// A CEF browser hosting a built-in plug-in's editor.
    WebView,
}

/// Resolved once: this is asked from the window procedure, on every paint.
fn debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some())
}

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::{Mutex, Once};

    use super::{debug_enabled, ContentHostKind, ContentRect};
    use crate::components::transport_key::{self, TransportKeySource};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateSolidBrush, FillRect, GetStockObject, BLACK_BRUSH, HBRUSH, HDC,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, GetKeyState, SetFocus};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, GetAncestor, GetClassNameW,
        GetClientRect, GetDesktopWindow, GetParent, GetWindow, GetWindowLongPtrW, GetWindowRect,
        GetWindowThreadProcessId, IsChild, IsWindow, RegisterClassW, SetWindowLongPtrW,
        SetWindowPos, SetWindowsHookExW, UnhookWindowsHookEx, GA_PARENT, GWLP_HWNDPARENT,
        GWL_STYLE, GW_CHILD, GW_OWNER, HC_ACTION, HHOOK, HMENU, HWND_TOPMOST, MSG, PM_REMOVE,
        SWP_NOACTIVATE, SWP_NOZORDER, WH_GETMESSAGE, WINDOW_EX_STYLE, WM_ERASEBKGND, WM_KEYDOWN,
        WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_NULL, WM_PARENTNOTIFY, WM_RBUTTONDOWN, WM_SETFOCUS,
        WM_SYSKEYDOWN, WM_XBUTTONDOWN, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
        WS_POPUP, WS_VISIBLE,
    };

    /// Every live content host and what it holds, so the hook can tell a key
    /// aimed at an embedded plug-in view from one aimed at the app's own UI —
    /// and a native view, whose keys it must claim, from a web view, whose keys
    /// CEF decides about.
    static HOSTS: Mutex<Vec<(u64, ContentHostKind)>> = Mutex::new(Vec::new());

    /// The installed `WH_GETMESSAGE` hook as a raw `HHOOK`, or 0 for none.
    static HOOK: AtomicIsize = AtomicIsize::new(0);

    /// Whether this message is a bare Space aimed at an embedded plug-in view.
    ///
    /// The same rule the editor shells apply
    /// (`native_editor_shell::claim_transport_key`): a fresh, unmodified Space
    /// only, and never one aimed at a text caret.
    ///
    /// # Safety
    ///
    /// `msg` must be a message the hook was handed for this thread's queue.
    unsafe fn claims_transport(msg: &MSG) -> bool {
        const VK_SPACE_W: usize = 0x20;
        const KEY_REPEAT_BIT: isize = 1 << 30;
        if msg.message != WM_KEYDOWN && msg.message != WM_SYSKEYDOWN {
            return false;
        }
        if msg.wParam.0 != VK_SPACE_W {
            return false;
        }
        // Auto-repeat: one press is one toggle, whatever the repeat rate is.
        if msg.lParam.0 & KEY_REPEAT_BIT != 0 {
            return false;
        }
        unsafe {
            let held = |vk: i32| GetKeyState(vk) < 0;
            // VK_CONTROL / VK_MENU / VK_LWIN / VK_RWIN.
            if held(0x11) || held(0x12) || held(0x5B) || held(0x5C) {
                keyboard_trace(msg, "modifier held", None);
                return false;
            }
            // Only keys headed into an embedded plug-in view. Everywhere else in
            // the app GPUI already routes Space through the key bindings, and
            // claiming it here as well would toggle the transport twice.
            let target = msg.hwnd;
            let Some(kind) = owning_host(target) else {
                keyboard_trace(msg, "not inside a plug-in view", None);
                return false;
            };
            // CEF answers this one. It is the only thing that can see whether a
            // DOM text field has focus, and it reports the presses it decides
            // are the transport's through `request_global_play_pause`. Claiming
            // here as well would take Space away from the editor's own text
            // fields — every Win32 class test sees only `Chrome_*`.
            if kind == ContentHostKind::WebView {
                keyboard_trace(msg, "web view; CEF decides", Some(kind));
                return false;
            }
            // A caret owns Space — typing a note name must not start playback.
            let focus = GetFocus();
            if !focus.0.is_null() {
                let name = class_name_of(focus);
                if is_text_entry_class(&name) {
                    keyboard_trace(msg, "caret owns the key", Some(kind));
                    return false;
                }
            }
            keyboard_trace(msg, "claimed", Some(kind));
            true
        }
    }

    /// Window classes that own a text caret. Space belongs to them, never to the
    /// transport — typing a note name must not start playback.
    ///
    /// Deliberately a *class* test and not a "is this a plug-in" test: a plug-in
    /// editor is not a text field, and vetoing the whole window whenever one is
    /// focused is how Space stops working inside plug-in editors entirely.
    fn is_text_entry_class(class: &str) -> bool {
        class == "edit"
            || class == "combobox"
            || class.starts_with("richedit")
            || class.starts_with("windows.ui.core")
            || class.contains("textbox")
    }

    /// The content host `target` belongs to, if any.
    ///
    /// Walks parents *and* owners. `IsChild` alone is not enough: a VST3 view is
    /// supposed to be a `WS_CHILD` of the handle it was attached to, but plug-ins
    /// routinely put parts of their UI — menus, note editors, tooltips — in
    /// popups that are *owned* by that window rather than children of it, and a
    /// key pressed in one of those is still a key pressed in the plug-in.
    unsafe fn owning_host(target: HWND) -> Option<ContentHostKind> {
        let Ok(hosts) = HOSTS.lock() else {
            return None;
        };
        if hosts.is_empty() {
            return None;
        }
        // SAFETY: a plain query with no arguments.
        let desktop = unsafe { GetDesktopWindow() };
        let mut hwnd = target;
        // Bounded: a window tree deep enough to exhaust this is not one of ours,
        // and an owner cycle would otherwise hang the message loop.
        for _ in 0..32 {
            if hwnd.0.is_null() || hwnd == desktop {
                return None;
            }
            if let Some(&(_, kind)) = hosts.iter().find(|(host, _)| hwnd_from(*host) == hwnd) {
                return Some(kind);
            }
            // A child's parent, or a top-level window's owner — and which of
            // those to ask has to be decided from the style. `GetAncestor` does
            // not follow owners by design, and for a top-level window it answers
            // the *desktop* rather than null, so asking it first and falling
            // back to the owner "if that was null" never reaches the owner at
            // all: the walk stops one step short every single time, which is the
            // step a plug-in's menu or floating editor needs.
            //
            // SAFETY: `hwnd` is checked non-null above, and every call here
            // tolerates a window destroyed since by answering null/zero.
            let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
            let next = if style & (WS_CHILD.0 as isize) != 0 {
                unsafe { GetAncestor(hwnd, GA_PARENT) }
            } else {
                unsafe { GetWindow(hwnd, GW_OWNER).unwrap_or_default() }
            };
            if next == hwnd {
                return None;
            }
            hwnd = next;
        }
        None
    }

    /// Lower-cased window class of `hwnd`, for diagnostics and the caret test.
    unsafe fn class_name_of(hwnd: HWND) -> String {
        let mut class = [0u16; 128];
        // SAFETY: the buffer outlives the call and the length is honoured.
        let len = unsafe { GetClassNameW(hwnd, &mut class) };
        if len <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&class[..len as usize]).to_ascii_lowercase()
    }

    /// Trace for the transport-key path.
    ///
    /// Every step of it is invisible from outside: the hook either sees a key or
    /// it does not, and the window it lands on either belongs to a plug-in view
    /// or does not. "Space does nothing" cannot distinguish those, so this says
    /// which window the key landed on, what that window is, and who was given
    /// the key.
    ///
    /// Gated, except for the first press the hook declines — one line saying why
    /// the first Space went nowhere is worth more than a hundred saying it again,
    /// and it is the line somebody reporting this will already have.
    unsafe fn keyboard_trace(msg: &MSG, verdict: &str, kind: Option<ContentHostKind>) {
        static FIRST_DECLINE: AtomicBool = AtomicBool::new(false);
        let claimed = verdict == "claimed";
        if !transport_key::key_debug()
            && !debug_enabled()
            && (claimed || FIRST_DECLINE.swap(true, Ordering::Relaxed))
        {
            return;
        }
        unsafe {
            let focus = GetFocus();
            let mut pid = 0u32;
            let tid = GetWindowThreadProcessId(msg.hwnd, Some(&mut pid));
            eprintln!(
                "[Keyboard] studio msg=0x{:04x} hwnd=0x{:x} class='{}' focus=0x{:x} \
                 focus_class='{}' host={} pid={pid} tid={tid} time={} repeat={} verdict={verdict}",
                msg.message,
                msg.hwnd.0 as u64,
                class_name_of(msg.hwnd),
                focus.0 as u64,
                class_name_of(focus),
                match kind {
                    Some(ContentHostKind::NativeView) => "native-view",
                    Some(ContentHostKind::WebView) => "web-view",
                    None => "none",
                },
                msg.time,
                msg.lParam.0 & (1 << 30) != 0,
            );
        }
    }

    /// Claims the transport key before the plug-in's own window procedure runs.
    ///
    /// A native child owns every key that reaches it: `WM_KEYDOWN` does not
    /// bubble to a parent, and this host deliberately routes focus *into* the
    /// plug-in so its text fields work. Without this, Space is Melodyne's and
    /// the DAW transport is unreachable while its editor is up.
    unsafe extern "system" fn transport_key_hook(
        code: i32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if code == HC_ACTION as i32 && wparam.0 as u32 == PM_REMOVE.0 {
            let msg = lparam.0 as *mut MSG;
            if !msg.is_null() {
                // SAFETY: for `HC_ACTION` the hook is handed a live `MSG` it is
                // explicitly allowed to modify.
                let msg = unsafe { &mut *msg };
                if unsafe { claims_transport(msg) } {
                    // The message time identifies this physical press in every
                    // process that sees it, so the router can recognise the
                    // plug-in host reporting the same one.
                    transport_key::claim(TransportKeySource::EmbeddedView, Some(msg.time));
                    // Swallowed rather than passed on, whether or not the router
                    // took it: the press is spoken for either way, and letting
                    // a deduplicated one through would type a space into the
                    // plug-in instead.
                    msg.message = WM_NULL;
                    msg.wParam = WPARAM(0);
                    msg.lParam = LPARAM(0);
                }
            }
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// Starts watching this thread's messages once the first host exists.
    ///
    /// `WH_GETMESSAGE` is thread-local, so it sees only the UI thread's own
    /// queue — the thread the plug-in's window lives on. That is the difference
    /// from the system-wide low-level hook the out-of-process plug-in host
    /// needs: this one cannot stall typing anywhere else on the machine.
    ///
    /// It also means this hook covers exactly the in-process editors (ARA,
    /// in-process VST3, CEF): a key headed for a window the separated host owns
    /// is queued on *that* process's thread and never appears here, which is
    /// why that process has to claim its own.
    fn register_host(hwnd: u64, kind: ContentHostKind) {
        let Ok(mut hosts) = HOSTS.lock() else {
            return;
        };
        hosts.push((hwnd, kind));
        if HOOK.load(Ordering::Acquire) != 0 {
            return;
        }
        // SAFETY: a thread-local hook on the calling thread, unhooked in
        // `unregister_host` once the last content host is gone.
        let installed = unsafe {
            SetWindowsHookExW(
                WH_GETMESSAGE,
                Some(transport_key_hook),
                None,
                GetCurrentThreadId(),
            )
        };
        match installed {
            Ok(hook) if !hook.is_invalid() => {
                HOOK.store(hook.0 as isize, Ordering::Release);
                // Not gated: whether the one path by which Space can reach the
                // transport from inside a plug-in exists at all is worth a line.
                eprintln!(
                    "[Keyboard] studio transport hook installed on thread {} for host 0x{hwnd:x}",
                    // SAFETY: a plain query with no arguments.
                    unsafe { GetCurrentThreadId() }
                );
            }
            _ => eprintln!(
                "[Keyboard] WARNING could not install the studio transport key hook; \
                 Space will not reach the transport from an embedded editor"
            ),
        }
    }

    fn unregister_host(hwnd: u64) {
        let Ok(mut hosts) = HOSTS.lock() else {
            return;
        };
        if let Some(index) = hosts.iter().position(|&(entry, _)| entry == hwnd) {
            hosts.remove(index);
        }
        if !hosts.is_empty() {
            return;
        }
        let raw = HOOK.swap(0, Ordering::AcqRel);
        if raw != 0 {
            // SAFETY: the handle came from the `SetWindowsHookExW` above and is
            // released exactly once.
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void)) };
        }
    }

    fn hwnd_from(handle: u64) -> HWND {
        HWND(handle as *mut core::ffi::c_void)
    }

    /// Dedicated window class for the content host. We do NOT use the predefined
    /// `STATIC` class: a blank static control fills its client area with a
    /// light/system background, which appears as the "blank white" the plugin
    /// editor showed before the host's view painted (spec Part 5). This class
    /// paints solid black instead, matching the host's embed child, so there is
    /// no white flash and any area outside the plugin's own view stays dark.
    const CONTENT_HOST_CLASS: PCWSTR = w!("SpherePluginContentHost");

    fn ensure_content_host_class() {
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(content_host_wndproc),
                lpszClassName: CONTENT_HOST_CLASS,
                hbrBackground: HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0),
                ..Default::default()
            };
            unsafe { RegisterClassW(&wc) };
        });
    }

    /// Fill colour for the host's uncovered area.
    ///
    /// Black in normal use so an editor never flashes light. Under the view
    /// trace it is magenta instead: "the panel is dark" cannot distinguish a
    /// host window that is composited but empty from one the parent painted
    /// straight over, and that distinction is the whole question when a native
    /// child does not appear.
    fn host_fill_brush() -> HBRUSH {
        if debug_enabled() {
            static MAGENTA: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            let handle = *MAGENTA.get_or_init(|| {
                let brush = unsafe { CreateSolidBrush(COLORREF(0x00FF00FF)) };
                brush.0 as usize
            });
            if handle != 0 {
                return HBRUSH(handle as *mut core::ffi::c_void);
            }
        }
        HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0)
    }

    /// Route Win32 keyboard focus to the embedded child view (the CEF browser
    /// or a VST3 editor).
    ///
    /// The GPUI shell never hands keyboard focus to native children on its
    /// own: its top-level HWND keeps focus through activation, so a text
    /// field inside an embedded editor could be clicked but never typed into.
    /// Two signals close that gap:
    ///
    /// - `WM_PARENTNOTIFY` with a button-down event — the one notification
    ///   this host reliably receives when a click lands anywhere inside the
    ///   embedded child, even though the click itself is consumed by the
    ///   child's own window procedure.
    /// - `WM_SETFOCUS` — anything that focuses this host (tabbing, an
    ///   explicit `SetFocus` from the shell) is really meant for the view
    ///   inside it.
    ///
    /// Both forward focus to the first child; Chromium (and VST3 views)
    /// route it on to their inner widget from there.
    unsafe fn focus_embedded_child(hwnd: HWND) {
        if let Ok(child) = unsafe { GetWindow(hwnd, GW_CHILD) } {
            let _ = unsafe { SetFocus(Some(child)) };
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[plugin-content-hwnd] keyboard focus routed to embedded child=0x{:x}",
                    child.0 as u64
                );
            }
        }
    }

    /// Suppress the default white erase: fill the content host's client area
    /// black so the plugin's WS_CHILD view (created by the host process) is never
    /// preceded by a white flash, and any uncovered region stays dark. WS_CLIPCHILDREN
    /// on this window keeps us from painting over the plugin's own child.
    unsafe extern "system" fn content_host_wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_PARENTNOTIFY {
            let event = (wparam.0 as u32) & 0xFFFF;
            if matches!(
                event,
                WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN | WM_XBUTTONDOWN
            ) {
                unsafe { focus_embedded_child(hwnd) };
            }
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        if msg == WM_SETFOCUS {
            unsafe { focus_embedded_child(hwnd) };
            return LRESULT(0);
        }
        if msg == WM_ERASEBKGND {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let mut rc = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut rc) };
            let brush = host_fill_brush();
            unsafe { FillRect(hdc, &rc, brush) };
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[plugin-content-hwnd] WM_ERASEBKGND suppressed=true fill={}",
                    if debug_enabled() { "magenta" } else { "black" }
                );
            }
            return LRESULT(1);
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Make `popup_hwnd` an owned, topmost window of `owner_hwnd` and place it
    /// at the given **physical** screen rectangle.
    ///
    /// A host window that parks a plug-in's `WS_CHILD` view over its own client
    /// area cannot draw a menu over that view — `WS_CLIPCHILDREN` removes the
    /// child's rectangle from the parent's visible region, which is exactly why
    /// the GPUI editor window sets it. The menu therefore has to be a window,
    /// and a window that drops from another one has to be *owned* by it:
    /// Windows keeps an owned window above its owner, keeps it off the taskbar
    /// and out of Alt-Tab, and destroys it with the owner. GPUI's
    /// `WindowKind::PopUp` supplies none of that (it creates the window with
    /// `WS_EX_TOOLWINDOW`, no owner and no topmost bit), and the editor it drops
    /// from is `WS_EX_TOPMOST`, so without this the popup is created, activated,
    /// drawn — and sits underneath.
    ///
    /// The rectangle is applied here too, in physical pixels supplied by the
    /// caller, because a popup's own DPI is sampled at the `CW_USEDEFAULT`
    /// position it was created at rather than the monitor it belongs on.
    ///
    /// Returns `false` when either handle is not a live window.
    pub fn place_owned_popup(
        popup_hwnd: u64,
        owner_hwnd: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> bool {
        if popup_hwnd == 0 || owner_hwnd == 0 || popup_hwnd == owner_hwnd {
            return false;
        }
        let popup = hwnd_from(popup_hwnd);
        let owner = hwnd_from(owner_hwnd);
        // Safety: both handles are validated as live windows first, and every
        // call below is a plain window-manager operation on the UI thread.
        unsafe {
            if !IsWindow(Some(popup)).as_bool() || !IsWindow(Some(owner)).as_bool() {
                return false;
            }
            SetWindowLongPtrW(popup, GWLP_HWNDPARENT, owner.0 as isize);
            let _ = SetWindowPos(
                popup,
                Some(HWND_TOPMOST),
                x,
                y,
                width.max(1),
                height.max(1),
                SWP_NOACTIVATE,
            );
        }
        eprintln!(
            "[plugin-editor-window] popup_owner_applied owner_hwnd=0x{owner_hwnd:x} \
             popup_hwnd=0x{popup_hwnd:x} rect=({x},{y},{width}x{height}) topmost=1"
        );
        true
    }

    /// A real `WS_CHILD` content window parented to a main-app top HWND. Drop
    /// destroys it. The handle ([`Self::hwnd`]) is what travels to the host
    /// process via `HostCommand::OpenEditorWithParentHwnd`.
    pub struct ContentChildHwnd {
        top_hwnd: u64,
        content_hwnd: u64,
    }

    /// Whether the native-child geometry trace is enabled.
    ///
    /// The docked ARA editor resizes with the panel drag, so this path runs
    /// per frame rather than once per window resize; `DESIGN.md` requires
    /// high-rate logs to be environment-gated.
    fn view_debug() -> bool {
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some())
    }

    impl ContentChildHwnd {
        fn log_diagnostics(&self, rect: ContentRect) {
            if !view_debug() {
                return;
            }
            unsafe {
                let top = hwnd_from(self.top_hwnd);
                let content = hwnd_from(self.content_hwnd);
                let parent = GetParent(content).map(|p| p.0 as u64).unwrap_or(0);
                let is_child = IsChild(top, content).as_bool();
                let style = GetWindowLongPtrW(content, GWL_STYLE);
                let mut shell_rect = windows::Win32::Foundation::RECT::default();
                let mut content_screen = windows::Win32::Foundation::RECT::default();
                let mut content_client = windows::Win32::Foundation::RECT::default();
                let _ = GetWindowRect(top, &mut shell_rect);
                let _ = GetWindowRect(content, &mut content_screen);
                let _ = GetClientRect(content, &mut content_client);
                eprintln!("[plugin-editor-window] shell_hwnd=0x{:x}", self.top_hwnd);
                eprintln!(
                    "[plugin-editor-window] content_hwnd=0x{:x}",
                    self.content_hwnd
                );
                eprintln!("[plugin-editor-window] GetParent(content_hwnd)=0x{parent:x}");
                eprintln!("[plugin-editor-window] content_is_child={is_child}");
                eprintln!(
                    "[plugin-editor-window] content_style=0x{style:08x} WS_CHILD={} WS_VISIBLE={}",
                    (style & WS_CHILD.0 as isize) != 0,
                    (style & WS_VISIBLE.0 as isize) != 0
                );
                eprintln!(
                    "[plugin-editor-window] shell_screen_rect=({},{},{},{})",
                    shell_rect.left, shell_rect.top, shell_rect.right, shell_rect.bottom
                );
                eprintln!(
                    "[plugin-editor-window] content_screen_rect=({},{},{},{})",
                    content_screen.left,
                    content_screen.top,
                    content_screen.right,
                    content_screen.bottom
                );
                eprintln!(
                    "[plugin-editor-window] content_client_rect=({}, {}, {}x{})",
                    rect.x, rect.y, rect.width, rect.height
                );
            }
        }

        /// Create the content child window under `top_hwnd` for a plug-in's own
        /// native view. Returns `None` if `top_hwnd` is not a window or window
        /// creation fails.
        pub fn create(top_hwnd: u64, rect: ContentRect) -> Option<Self> {
            Self::create_for(ContentHostKind::NativeView, top_hwnd, rect)
        }

        /// As [`Self::create`], saying what the host will hold.
        ///
        /// The kind decides who owns the transport key inside it — see
        /// [`ContentHostKind`].
        pub fn create_for(kind: ContentHostKind, top_hwnd: u64, rect: ContentRect) -> Option<Self> {
            if top_hwnd == 0 {
                return None;
            }
            let top = hwnd_from(top_hwnd);
            ensure_content_host_class();
            // Safety: all args are validated; the content host class is
            // registered above. The plugin paints its own child into this HWND
            // after the host attaches the view; this window only provides a
            // black, non-erasing backing (no white flash — spec Part 5).
            unsafe {
                if !IsWindow(Some(top)).as_bool() {
                    return None;
                }
                let content = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    CONTENT_HOST_CLASS,
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                    rect.x,
                    rect.y,
                    rect.width.max(1),
                    rect.height.max(1),
                    Some(top),
                    None::<HMENU>,
                    None,
                    None,
                )
                .ok()?;

                let content_u64 = content.0 as u64;
                if content_u64 == top_hwnd {
                    // Must never happen with a child window; bail rather than
                    // letting the host attach to the top window.
                    let _ = DestroyWindow(content);
                    return None;
                }

                let result = Self {
                    top_hwnd,
                    content_hwnd: content_u64,
                };
                register_host(content_u64, kind);
                if debug_enabled() {
                    eprintln!("[PluginEditorWindow] content_hwnd != top_hwnd");
                }
                result.log_diagnostics(rect);
                Some(result)
            }
        }

        /// The content child HWND as a `u64` — the value passed to the host.
        pub fn hwnd(&self) -> u64 {
            self.content_hwnd
        }

        pub fn top_hwnd(&self) -> u64 {
            self.top_hwnd
        }

        /// Resize/reposition the content child (main app owns the geometry; it
        /// then tells the host to re-issue `onSize`).
        pub fn set_bounds(&self, rect: ContentRect) {
            unsafe {
                let _ = SetWindowPos(
                    hwnd_from(self.content_hwnd),
                    None,
                    rect.x,
                    rect.y,
                    rect.width.max(1),
                    rect.height.max(1),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if view_debug() {
                eprintln!(
                    "[plugin-editor-window] resize content_hwnd=0x{:x} rect=({},{},{}x{})",
                    self.content_hwnd, rect.x, rect.y, rect.width, rect.height
                );
            }
            self.log_diagnostics(rect);
        }

        /// True while the content HWND is still a valid window.
        pub fn is_valid(&self) -> bool {
            unsafe { IsWindow(Some(hwnd_from(self.content_hwnd))).as_bool() }
        }
    }

    impl Drop for ContentChildHwnd {
        fn drop(&mut self) {
            unregister_host(self.content_hwnd);
            unsafe {
                if self.content_hwnd != 0 && IsWindow(Some(hwnd_from(self.content_hwnd))).as_bool()
                {
                    let _ = DestroyWindow(hwnd_from(self.content_hwnd));
                }
            }
        }
    }

    /// A never-shown top-level HWND for a native child that must exist without
    /// being visible — the boot-time CEF warm-up browser, which only has to
    /// spawn Chromium's helper processes. `WS_POPUP` without `WS_VISIBLE`, no
    /// owner, so it never appears on screen or the taskbar. Drop destroys it.
    pub struct HiddenHostWindow {
        hwnd: u64,
    }

    impl HiddenHostWindow {
        pub fn create() -> Option<Self> {
            ensure_content_host_class();
            // SAFETY: the class is registered above; every argument is a
            // constant or null, and the handle is destroyed on drop.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    CONTENT_HOST_CLASS,
                    PCWSTR::null(),
                    WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                    0,
                    0,
                    2,
                    2,
                    None,
                    None::<HMENU>,
                    None,
                    None,
                )
            }
            .ok()?;
            Some(Self {
                hwnd: hwnd.0 as u64,
            })
        }

        pub fn hwnd(&self) -> u64 {
            self.hwnd
        }
    }

    impl Drop for HiddenHostWindow {
        fn drop(&mut self) {
            unsafe {
                if self.hwnd != 0 && IsWindow(Some(hwnd_from(self.hwnd))).as_bool() {
                    let _ = DestroyWindow(hwnd_from(self.hwnd));
                }
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::{ContentHostKind, ContentRect};

    /// Non-Windows stub: off-screen hosting never needs a hidden parent.
    pub struct HiddenHostWindow {
        _private: (),
    }

    impl HiddenHostWindow {
        pub fn create() -> Option<Self> {
            None
        }
        pub fn hwnd(&self) -> u64 {
            0
        }
    }

    /// Non-Windows stub. Host-process editor embedding via NSView/X11 is a later
    /// slice; this keeps the crate compiling everywhere.
    pub struct ContentChildHwnd {
        _private: (),
    }

    /// No native child is ever created off Windows, so nothing occludes an
    /// ordinary popup and there is no owner to attach.
    pub fn place_owned_popup(
        _popup_hwnd: u64,
        _owner_hwnd: u64,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> bool {
        false
    }

    impl ContentChildHwnd {
        pub fn create(_top_hwnd: u64, _rect: ContentRect) -> Option<Self> {
            None
        }
        pub fn create_for(
            _kind: ContentHostKind,
            _top_hwnd: u64,
            _rect: ContentRect,
        ) -> Option<Self> {
            None
        }
        pub fn hwnd(&self) -> u64 {
            0
        }
        pub fn top_hwnd(&self) -> u64 {
            0
        }
        pub fn set_bounds(&self, _rect: ContentRect) {}
        pub fn is_valid(&self) -> bool {
            false
        }
    }
}

pub use imp::{place_owned_popup, ContentChildHwnd, HiddenHostWindow};
