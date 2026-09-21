//! Browser-process CEF client for plugin editor browsers.
//!
//! The client locks navigation to the editor's initial origin, records the full
//! browser/load lifecycle, and exposes thread-safe state to the native host so
//! registry insertion/removal can follow `OnAfterCreated`/`OnBeforeClose`.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use cef::rc::Rc as _;
use cef::{
    wrap_client, wrap_display_handler, wrap_keyboard_handler, wrap_life_span_handler,
    wrap_load_handler, wrap_request_handler, Browser, BrowserSettings, CefString, CefStringUtf16,
    Client, DictionaryValue, DisplayHandler, Errorcode, Frame, ImplBrowser, ImplClient,
    ImplDisplayHandler, ImplFrame, ImplKeyboardHandler, ImplLifeSpanHandler, ImplLoadHandler,
    ImplRequest, ImplRequestHandler, KeyEvent, KeyEventType, KeyboardHandler, LifeSpanHandler,
    LoadHandler, LogSeverity, PopupFeatures, RenderHandler, Request, RequestHandler,
    TerminationStatus, TransitionType, WindowInfo, WindowOpenDisposition, WrapClient,
    WrapDisplayHandler, WrapKeyboardHandler, WrapLifeSpanHandler, WrapLoadHandler,
    WrapRequestHandler,
};

use crate::scheme::{cef_diagnostics_enabled, PLUGIN_SCHEME};

const JAVASCRIPT_PROBE: &str =
    "console.log('[cef-diagnostic] javascript-executed url=' + location.href);";

static NEXT_CLIENT_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ObjectLifetime {
    _inner: Arc<ObjectLifetimeInner>,
}

struct ObjectLifetimeInner {
    object_type: &'static str,
    object_id: u64,
}

impl ObjectLifetime {
    fn new(object_type: &'static str) -> Self {
        let object_id = NEXT_CLIENT_OBJECT_ID.fetch_add(1, Ordering::Relaxed);
        if cef_diagnostics_enabled() {
            eprintln!(
                "[cef-ref] object_type={object_type} object_id={object_id} event=created thread={:?}",
                std::thread::current().id()
            );
        }
        Self {
            _inner: Arc::new(ObjectLifetimeInner {
                object_type,
                object_id,
            }),
        }
    }
}

impl Drop for ObjectLifetimeInner {
    fn drop(&mut self) {
        if cef_diagnostics_enabled() {
            eprintln!(
                "[cef-ref] object_type={} object_id={} event=final_destruction thread={:?}",
                self.object_type,
                self.object_id,
                std::thread::current().id()
            );
        }
    }
}

#[derive(Default)]
struct BrowserLifecycleInner {
    after_created: AtomicBool,
    before_close: AtomicBool,
    renderer_terminated: AtomicBool,
    javascript_executed: AtomicBool,
    browser_id: AtomicI32,
    global_play_pause_requests: AtomicU32,
}

/// CEF lifecycle state shared by callbacks and the native editor registry.
#[derive(Clone, Default)]
pub struct BrowserLifecycle(Arc<BrowserLifecycleInner>);

impl BrowserLifecycle {
    pub fn after_created(&self) -> bool {
        self.0.after_created.load(Ordering::Acquire)
    }

    pub fn before_close(&self) -> bool {
        self.0.before_close.load(Ordering::Acquire)
    }

    pub fn browser_id(&self) -> Option<i32> {
        let id = self.0.browser_id.load(Ordering::Acquire);
        (id > 0).then_some(id)
    }

    pub fn javascript_executed(&self) -> bool {
        self.0.javascript_executed.load(Ordering::Acquire)
    }

    /// Read-and-clear the renderer termination signal.
    pub fn take_renderer_terminated(&self) -> bool {
        self.0.renderer_terminated.swap(false, Ordering::AcqRel)
    }

    /// Drain Space presses captured by CEF while focus is outside an editable
    /// field. The GPUI host dispatches these through the Studio command system.
    pub fn take_global_play_pause_requests(&self) -> u32 {
        self.0.global_play_pause_requests.swap(0, Ordering::AcqRel)
    }

    fn mark_after_created(&self, browser_id: i32) {
        self.0.browser_id.store(browser_id, Ordering::Release);
        self.0.after_created.store(true, Ordering::Release);
    }

    fn mark_before_close(&self) {
        self.0.before_close.store(true, Ordering::Release);
    }

    fn mark_renderer_terminated(&self) {
        self.0.renderer_terminated.store(true, Ordering::Release);
    }

    fn mark_javascript_executed(&self) {
        self.0.javascript_executed.store(true, Ordering::Release);
    }

    fn request_global_play_pause(&self) {
        self.0
            .global_play_pause_requests
            .fetch_add(1, Ordering::Release);
    }
}

fn browser_id(browser: Option<&mut Browser>) -> i32 {
    browser.map(|browser| browser.identifier()).unwrap_or(-1)
}

fn frame_url(frame: Option<&mut Frame>) -> String {
    frame
        .map(|frame| CefStringUtf16::from(&frame.url()).to_string())
        .unwrap_or_else(|| "<no-frame>".to_string())
}

fn cef_string(value: Option<&CefString>) -> String {
    value
        .map(ToString::to_string)
        .unwrap_or_else(|| "<none>".to_string())
}

wrap_life_span_handler! {
    pub struct PluginLifeSpanHandler {
        lifecycle: BrowserLifecycle,
        _lifetime: ObjectLifetime,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            let opened = user_gesture != 0 && open_external_https(&url);
            eprintln!(
                "[cef-lifecycle] event=PopupBlocked external_opened={opened}"
            );
            // Built-in plugin pages never own a second CEF browser. A valid,
            // user-initiated HTTPS link is handed to the native OS above.
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let id = browser_id(browser);
            self.lifecycle.mark_after_created(id);
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-lifecycle] event=OnAfterCreated browser_id={id} thread={:?}",
                    std::thread::current().id()
                );
                log::info!(
                    "event=OnAfterCreated browser_id={id} thread={:?}",
                    std::thread::current().id()
                );
            }
        }

        fn do_close(&self, browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            let id = browser_id(browser);
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-lifecycle] event=DoClose browser_id={id} return=false thread={:?}",
                    std::thread::current().id()
                );
            }
            0
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let id = browser_id(browser);
            self.lifecycle.mark_before_close();
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-lifecycle] event=OnBeforeClose browser_id={id} thread={:?}",
                    std::thread::current().id()
                );
                log::info!(
                    "event=OnBeforeClose browser_id={id} thread={:?}",
                    std::thread::current().id()
                );
            }
        }
    }
}

wrap_load_handler! {
    pub struct PluginLoadHandler {
        lifecycle: BrowserLifecycle,
        _lifetime: ObjectLifetime,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            let id = browser_id(browser);
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-lifecycle] event=OnLoadingStateChange browser_id={id} is_loading={} can_go_back={} can_go_forward={} thread={:?}",
                    is_loading != 0,
                    can_go_back != 0,
                    can_go_forward != 0,
                    std::thread::current().id()
                );
            }
        }

        fn on_load_start(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            transition_type: TransitionType,
        ) {
            let id = browser_id(browser);
            let url = frame_url(frame);
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-lifecycle] event=OnLoadStart browser_id={id} url={url:?} transition={transition_type:?} thread={:?}",
                    std::thread::current().id()
                );
            }
        }

        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            http_status_code: ::std::os::raw::c_int,
        ) {
            let id = browser_id(browser);
            let Some(frame) = frame else {
                if cef_diagnostics_enabled() {
                    eprintln!(
                        "[cef-lifecycle] event=OnLoadEnd browser_id={id} status={http_status_code} url=\"<no-frame>\" thread={:?}",
                        std::thread::current().id()
                    );
                }
                return;
            };
            let url = CefStringUtf16::from(&frame.url()).to_string();
            let is_main = frame.is_main() != 0;
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-lifecycle] event=OnLoadEnd browser_id={id} status={http_status_code} is_main={is_main} url={url:?} thread={:?}",
                    std::thread::current().id()
                );
            }
            if is_main {
                frame.execute_java_script(
                    Some(&CefString::from(JAVASCRIPT_PROBE)),
                    Some(&CefString::from("futureboard-cef-diagnostic")),
                    1,
                );
            }
        }

        fn on_load_error(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let id = browser_id(browser);
            let frame_url = frame_url(frame);
            let error_text = cef_string(error_text);
            let failed_url = cef_string(failed_url);
            eprintln!(
                "[cef-lifecycle] event=OnLoadError browser_id={id} code={error_code:?} error={error_text:?} failed_url={failed_url:?} frame_url={frame_url:?} thread={:?}",
                std::thread::current().id()
            );
        }
    }
}

wrap_display_handler! {
    pub struct PluginDisplayHandler {
        lifecycle: BrowserLifecycle,
        _lifetime: ObjectLifetime,
    }

    impl DisplayHandler {
        fn on_console_message(
            &self,
            browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let id = browser_id(browser);
            let message = cef_string(message);
            let source = cef_string(source);
            if message.contains("[cef-diagnostic] javascript-executed") {
                self.lifecycle.mark_javascript_executed();
            }
            if cef_diagnostics_enabled() || message.contains("[cef-diagnostic]") {
                eprintln!(
                    "[cef-console] browser_id={id} level={level:?} source={source:?} line={line} message={message:?} thread={:?}",
                    std::thread::current().id()
                );
            }
            0
        }
    }
}

wrap_request_handler! {
    pub struct PluginRequestHandler {
        lifecycle: BrowserLifecycle,
        allowed_origin: String,
        control_url: Option<String>,
        _lifetime: ObjectLifetime,
    }

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(request) = request else { return 0 };
            let url = CefStringUtf16::from(&request.url()).to_string();
            match navigation_decision(
                &url,
                &self.allowed_origin,
                self.control_url.as_deref(),
                user_gesture != 0,
            ) {
                NavigationDecision::Allow => 0,
                NavigationDecision::OpenExternalHttps => {
                    let opened = open_external_https(&url);
                    eprintln!(
                        "[cef-lifecycle] event=ExternalNavigationIntercepted opened={opened}"
                    );
                    1
                }
                NavigationDecision::Block => {
                    eprintln!("[cef-lifecycle] event=NavigationBlocked");
                    1
                }
            }
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            error_string: Option<&CefString>,
        ) {
            let id = browser_id(browser);
            let error_string = cef_string(error_string);
            eprintln!(
                "[cef-lifecycle] event=OnRenderProcessTerminated browser_id={id} status={status:?} error_code={error_code} error={error_string:?} thread={:?}",
                std::thread::current().id()
            );
            self.lifecycle.mark_renderer_terminated();
        }
    }
}

fn plugin_origin(url: &str) -> Option<String> {
    let prefix = format!("{PLUGIN_SCHEME}://");
    if !url.to_ascii_lowercase().starts_with(&prefix) {
        return None;
    }
    let rest = &url[prefix.len()..];
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..host_end];
    (!host.is_empty()).then(|| format!("{PLUGIN_SCHEME}://{}", host.to_ascii_lowercase()))
}

fn is_allowed_navigation(url: &str, allowed_origin: &str, control_url: Option<&str>) -> bool {
    if url.eq_ignore_ascii_case("about:blank") {
        return true;
    }
    if control_url.is_some_and(|control| url == control) {
        return true;
    }
    let lower = url.to_ascii_lowercase();
    lower == allowed_origin || lower.starts_with(&format!("{allowed_origin}/"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationDecision {
    Allow,
    OpenExternalHttps,
    Block,
}

fn navigation_decision(
    url: &str,
    allowed_origin: &str,
    control_url: Option<&str>,
    user_gesture: bool,
) -> NavigationDecision {
    if is_allowed_navigation(url, allowed_origin, control_url) {
        NavigationDecision::Allow
    } else if user_gesture && is_safe_external_https(url) {
        NavigationDecision::OpenExternalHttps
    } else {
        NavigationDecision::Block
    }
}

fn is_safe_external_https(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

#[cfg(target_os = "macos")]
fn open_external_https(url: &str) -> bool {
    use std::process::{Command, Stdio};

    is_safe_external_https(url)
        && Command::new("/usr/bin/open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
}

#[cfg(not(target_os = "macos"))]
fn open_external_https(_url: &str) -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginKeyAction {
    ConsumeZoom,
    GlobalPlayPause,
}

fn plugin_key_action(event: &KeyEvent) -> Option<PluginKeyAction> {
    // CEF may follow RAWKEYDOWN with CHAR. Handling only RAWKEYDOWN prevents
    // one physical Space press from toggling transport twice.
    if event.type_ != KeyEventType::RAWKEYDOWN {
        return None;
    }

    const CONTROL_DOWN: u32 = 4;
    const ALT_DOWN: u32 = 8;
    const COMMAND_DOWN: u32 = 128;
    const IS_REPEAT: u32 = 8192;
    const VKEY_SPACE: i32 = 0x20;
    const VKEY_0: i32 = 0x30;
    const VKEY_ADD: i32 = 0x6b;
    const VKEY_SUBTRACT: i32 = 0x6d;
    const VKEY_OEM_PLUS: i32 = 0xbb;
    const VKEY_OEM_MINUS: i32 = 0xbd;

    let primary = event.modifiers & (CONTROL_DOWN | COMMAND_DOWN) != 0;
    let zoom_key = matches!(
        event.windows_key_code,
        VKEY_0 | VKEY_ADD | VKEY_SUBTRACT | VKEY_OEM_PLUS | VKEY_OEM_MINUS
    );
    if primary && zoom_key {
        return Some(PluginKeyAction::ConsumeZoom);
    }

    let modified = event.modifiers & (CONTROL_DOWN | ALT_DOWN | COMMAND_DOWN) != 0;
    let repeated = event.modifiers & IS_REPEAT != 0;
    if event.windows_key_code == VKEY_SPACE
        && !modified
        && !repeated
        && event.focus_on_editable_field == 0
    {
        return Some(PluginKeyAction::GlobalPlayPause);
    }
    None
}

macro_rules! plugin_keyboard_handler {
    ($os_event:ty) => {
        wrap_keyboard_handler! {
            pub struct PluginKeyboardHandler {
                lifecycle: BrowserLifecycle,
                _lifetime: ObjectLifetime,
            }

            impl KeyboardHandler {
                fn on_pre_key_event(
                    &self,
                    _browser: Option<&mut Browser>,
                    event: Option<&KeyEvent>,
                    _os_event: $os_event,
                    _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
                ) -> ::std::os::raw::c_int {
                    match event.and_then(plugin_key_action) {
                        Some(PluginKeyAction::GlobalPlayPause) => {
                            self.lifecycle.request_global_play_pause();
                            1
                        }
                        Some(PluginKeyAction::ConsumeZoom) => 1,
                        None => 0,
                    }
                }
            }
        }
    };
}

#[cfg(windows)]
plugin_keyboard_handler!(Option<&mut cef::sys::MSG>);
#[cfg(target_os = "linux")]
plugin_keyboard_handler!(Option<&mut cef::sys::XEvent>);
#[cfg(target_os = "macos")]
plugin_keyboard_handler!(*mut u8);

wrap_client! {
    pub struct PluginBrowserClient {
        request_handler: RequestHandler,
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
        display_handler: DisplayHandler,
        keyboard_handler: KeyboardHandler,
        // `Some` only for a windowless browser; returning `None` here is what
        // tells CEF the browser is windowed.
        render_handler: Option<RenderHandler>,
        _lifetime: ObjectLifetime,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            self.render_handler.clone()
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request_handler.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(self.keyboard_handler.clone())
        }
    }
}

/// Build one explicitly retained client and its observable lifecycle state.
pub fn plugin_browser_client(initial_url: &str) -> (Client, BrowserLifecycle) {
    plugin_browser_client_with_surface(initial_url, None)
}

/// As [`plugin_browser_client`], but for a windowless browser: `surface` is
/// the off-screen framebuffer CEF paints into. Passing `Some` here and
/// [`crate::runtime::WebViewConfig::windowless`] at creation are two halves of
/// the same decision — a windowless browser without a render handler never
/// paints, and a windowed browser with one is rejected by CEF.
pub fn plugin_browser_client_with_surface(
    initial_url: &str,
    surface: Option<crate::osr::OsrSurface>,
) -> (Client, BrowserLifecycle) {
    let lifecycle = BrowserLifecycle::default();
    let control_url = (!initial_url
        .to_ascii_lowercase()
        .starts_with(&format!("{PLUGIN_SCHEME}://")))
    .then(|| initial_url.to_string());
    let allowed_origin = plugin_origin(initial_url).unwrap_or_default();

    let request_handler = PluginRequestHandler::new(
        lifecycle.clone(),
        allowed_origin,
        control_url,
        ObjectLifetime::new("cef_resource_request_handler_t"),
    );
    let life_span_handler = PluginLifeSpanHandler::new(
        lifecycle.clone(),
        ObjectLifetime::new("cef_life_span_handler_t"),
    );
    let load_handler =
        PluginLoadHandler::new(lifecycle.clone(), ObjectLifetime::new("cef_load_handler_t"));
    let display_handler = PluginDisplayHandler::new(
        lifecycle.clone(),
        ObjectLifetime::new("cef_display_handler_t"),
    );
    let keyboard_handler = PluginKeyboardHandler::new(
        lifecycle.clone(),
        ObjectLifetime::new("cef_keyboard_handler_t"),
    );
    let render_handler = surface.map(crate::osr::osr_render_handler);
    let client = PluginBrowserClient::new(
        request_handler,
        life_span_handler,
        load_handler,
        display_handler,
        keyboard_handler,
        render_handler,
        ObjectLifetime::new("cef_client_t"),
    );
    if cef_diagnostics_enabled() {
        eprintln!(
            "[cef-ref] object_type=cef_client_t event=return_to_host has_one_ref={} thread={:?}",
            client.has_one_ref(),
            std::thread::current().id()
        );
    }
    (client, lifecycle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_only_the_expected_plugin_origin_and_about_blank() {
        let origin = "mikoplugin://rodharerist";
        assert!(is_allowed_navigation(
            "mikoplugin://rodharerist/index.html",
            origin,
            None
        ));
        assert!(is_allowed_navigation(
            "MIKOPLUGIN://RODHARERIST/assets/app.js",
            origin,
            None
        ));
        assert!(is_allowed_navigation("about:blank", origin, None));
        assert!(!is_allowed_navigation(
            "mikoplugin://another/index.html",
            origin,
            None
        ));
    }

    #[test]
    fn allows_only_the_exact_control_url() {
        let control = "data:text/html,<p>control</p>";
        assert!(is_allowed_navigation(control, "", Some(control)));
        assert!(!is_allowed_navigation(
            "https://example.com",
            "",
            Some(control)
        ));
    }

    #[test]
    fn blocks_everything_else() {
        for url in [
            "https://example.com",
            "http://localhost:1234",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "",
        ] {
            assert!(!is_allowed_navigation(
                url,
                "mikoplugin://rodharerist",
                None
            ));
        }
    }

    #[test]
    fn intercepts_only_user_initiated_safe_https_links() {
        let origin = "mikoplugin://rodharerist";
        assert_eq!(
            navigation_decision("https://futureboard.app/help", origin, None, true),
            NavigationDecision::OpenExternalHttps
        );
        for url in [
            "http://futureboard.app/help",
            "https://user:password@futureboard.app/help",
            "javascript:alert(1)",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                navigation_decision(url, origin, None, true),
                NavigationDecision::Block,
                "{url}"
            );
        }
        assert_eq!(
            navigation_decision("https://futureboard.app/help", origin, None, false),
            NavigationDecision::Block
        );
    }

    #[test]
    fn lifecycle_signals_read_and_clear() {
        let lifecycle = BrowserLifecycle::default();
        assert!(!lifecycle.after_created());
        lifecycle.mark_after_created(7);
        assert!(lifecycle.after_created());
        assert_eq!(lifecycle.browser_id(), Some(7));
        lifecycle.mark_renderer_terminated();
        assert!(lifecycle.take_renderer_terminated());
        assert!(!lifecycle.take_renderer_terminated());
        lifecycle.request_global_play_pause();
        lifecycle.request_global_play_pause();
        assert_eq!(lifecycle.take_global_play_pause_requests(), 2);
        assert_eq!(lifecycle.take_global_play_pause_requests(), 0);
    }

    #[test]
    fn plugin_keys_disable_zoom_and_route_unmodified_space() {
        let raw = |windows_key_code, modifiers, editable| KeyEvent {
            type_: KeyEventType::RAWKEYDOWN,
            windows_key_code,
            modifiers,
            focus_on_editable_field: i32::from(editable),
            ..Default::default()
        };
        assert_eq!(
            plugin_key_action(&raw(0x20, 0, false)),
            Some(PluginKeyAction::GlobalPlayPause)
        );
        assert_eq!(plugin_key_action(&raw(0x20, 0, true)), None);
        assert_eq!(plugin_key_action(&raw(0x20, 4, false)), None);
        assert_eq!(plugin_key_action(&raw(0x20, 8192, false)), None);
        for key in [0x30, 0x6b, 0x6d, 0xbb, 0xbd] {
            assert_eq!(
                plugin_key_action(&raw(key, 4, false)),
                Some(PluginKeyAction::ConsumeZoom)
            );
            assert_eq!(
                plugin_key_action(&raw(key, 128, true)),
                Some(PluginKeyAction::ConsumeZoom)
            );
        }
    }
}
