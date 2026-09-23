//! Custom-scheme hosting for embedded plugin editor UIs.
//!
//! A built-in plugin's compiled React editor lives in its own library as a
//! `&'static [u8]` table (see `builtin_audio_plugins::ui`). This module serves
//! that table to CEF under a custom scheme so the editor loads as a real
//! document with a proper origin, rather than through a `file://` path or a
//! `data:` URL.
//!
//! ## Registration order matters
//!
//! CEF requires a custom scheme to be declared in *every* process, from
//! `cef_app_t::on_register_custom_schemes`. That means the same [`App`] must be
//! handed to both [`crate::runtime::execute_subprocess`] and
//! [`crate::runtime::CefRuntime::initialize`] — a factory registered only in
//! the browser process will not make the scheme resolvable in the renderer.
//! [`plugin_scheme_app`] builds that app; [`register_plugin_scheme_factory`]
//! installs the handler and must be called *after* initialize succeeds.
//!
//! ## Threading
//!
//! Factory and handler methods are invoked on CEF's IO thread, not the UI
//! thread. The resolver is therefore `Send + Sync` and must not block: it is
//! only ever expected to index a static table.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
// The `wrap_*!` macros expand to `impl Impl<T> for` / `impl Wrap<T> for`
// blocks, so every one of these traits has to be nameable here.
use cef::wrapper::stream_resource_handler::StreamResourceHandler;
use cef::{
    wrap_app, wrap_browser_process_handler, wrap_resource_handler, wrap_scheme_handler_factory,
    App, BrowserProcessHandler, CefString, CefStringUtf16, ImplApp, ImplBrowserProcessHandler,
    ImplCommandLine, ImplPostData, ImplPostDataElement, ImplRequest, ImplResourceHandler,
    ImplResponse, ImplSchemeHandlerFactory, ImplSchemeRegistrar, ResourceHandler,
    SchemeHandlerFactory, SchemeOptions, WrapApp, WrapBrowserProcessHandler, WrapResourceHandler,
    WrapSchemeHandlerFactory,
};

/// Scheme built-in plugin editors are served under. Must match
/// `builtin_audio_plugins::ui::PLUGIN_URL_SCHEME`.
pub const PLUGIN_SCHEME: &str = "mikoplugin";

/// Chromium's macOS credential store is unnecessary for Futureboard's local,
/// ephemeral CEF renderer. The CEF API expects switch names without `--`.
pub const MACOS_CEF_USE_MOCK_KEYCHAIN: &str = "use-mock-keychain";
const CEF_DISABLE_PINCH: &str = "disable-pinch";
const CEF_DISABLE_GPU: &str = "disable-gpu";

fn runtime_flag_enabled(flag: &str) -> bool {
    std::env::args_os()
        .skip(1)
        .any(|arg| arg.to_string_lossy().eq_ignore_ascii_case(flag))
}

fn cef_gpu_isolation_enabled() -> bool {
    std::env::var_os("FUTUREBOARD_CEF_DISABLE_GPU").is_some()
}

fn cef_gpu_disabled() -> bool {
    runtime_flag_enabled("--disable-cef-gpu")
        || std::env::var_os("FUTUREBOARD_DISABLE_CEF_GPU").is_some()
        || cef_gpu_isolation_enabled()
}

/// `FUTUREBOARD_CEF_STATIC_TEST=1` serves one trivial document and never the
/// embedded React bundle.
pub fn cef_static_test_enabled() -> bool {
    std::env::var_os("FUTUREBOARD_CEF_STATIC_TEST").is_some()
}

/// React→native and native→React bridge traffic. Static-test mode implies off.
pub fn cef_ipc_enabled() -> bool {
    !cef_static_test_enabled() && std::env::var_os("FUTUREBOARD_CEF_DISABLE_IPC").is_none()
}

/// Extra Chromium switches for diagnostics and GPU isolation.
///
/// `disable-gpu-compositing` is paired with `disable-gpu` only for
/// `FUTUREBOARD_CEF_DISABLE_GPU`. The older disable-GPU flag leaves
/// compositing policy alone so the two modes stay distinguishable.
pub(crate) fn extra_diagnostic_switches(
    disable_gpu: bool,
    disable_gpu_compositing: bool,
    verbose_logging: bool,
) -> Vec<(&'static str, Option<&'static str>)> {
    let mut switches = Vec::new();
    if disable_gpu {
        switches.push((CEF_DISABLE_GPU, None));
    }
    if disable_gpu_compositing {
        switches.push(("disable-gpu-compositing", None));
    }
    if verbose_logging {
        switches.push(("enable-logging", None));
        switches.push(("v", Some("1")));
    }
    switches
}

fn local_ui_command_line_switches(target_os: &str) -> &'static [&'static str] {
    const COMMON: &[&str] = &[CEF_DISABLE_PINCH];
    const MACOS: &[&str] = &[CEF_DISABLE_PINCH, MACOS_CEF_USE_MOCK_KEYCHAIN];
    if target_os == "macos" {
        MACOS
    } else {
        COMMON
    }
}

/// Reserved path React's bridge client POSTs JSON envelopes to
/// (`futureboard.requestSelectInstance`, `instanceReady`, `bridgeReady`,
/// `rodhareist.setParam`, ...). Not a real asset path — intercepted before it
/// ever reaches the plugin's resolver, so no plugin's asset table needs to
/// know about it or can shadow it.
const BRIDGE_PATH: &str = "/__bridge";

const BRIDGE_ACK: SchemeAsset = SchemeAsset {
    bytes: b"{\"ok\":true}",
    mime_type: "application/json; charset=utf-8",
};

/// Under 100 bytes and self-verifying: the console message proves JavaScript ran.
const MINIMAL_TEST_DOCUMENT: SchemeAsset = SchemeAsset {
    bytes: b"<!doctype html><script>console.log('minimal-js-ok')</script><p>minimal</p>",
    mime_type: "text/html",
};

/// Isolation page for `FUTUREBOARD_CEF_STATIC_TEST`. No scripts, no router.
const STATIC_TEST_DOCUMENT: SchemeAsset = SchemeAsset {
    bytes: b"<html>\n<body>Futureboard CEF Test</body>\n</html>\n",
    mime_type: "text/html",
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ObjectLifetime {
    _inner: Arc<ObjectLifetimeInner>,
}

struct ObjectLifetimeInner {
    object_type: &'static str,
    object_id: u64,
}

impl ObjectLifetime {
    fn new(object_type: &'static str, object_id: u64) -> Self {
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

pub fn cef_diagnostics_enabled() -> bool {
    cfg!(debug_assertions)
        || std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some()
        || std::env::var_os("FUTUREBOARD_GPU_DIAGNOSTICS").is_some()
        || runtime_flag_enabled("--gpu-diagnostics")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceHandlerMode {
    BuiltIn,
    Custom,
}

fn resource_handler_mode() -> ResourceHandlerMode {
    match std::env::var("FUTUREBOARD_CEF_RESOURCE_HANDLER") {
        Ok(value) if value.eq_ignore_ascii_case("builtin") => ResourceHandlerMode::BuiltIn,
        _ => ResourceHandlerMode::Custom,
    }
}

fn diagnostic_document(path: &str, resolved: Option<SchemeAsset>) -> Option<SchemeAsset> {
    if path != "/index.html" {
        return resolved;
    }
    match std::env::var("FUTUREBOARD_CEF_TEST_DOCUMENT") {
        Ok(value) if value.eq_ignore_ascii_case("minimal") => Some(MINIMAL_TEST_DOCUMENT),
        // `large` deliberately means the current embedded single-file document.
        _ => resolved,
    }
}

fn make_resource_handler(asset: Option<SchemeAsset>, request_id: u64) -> ResourceHandler {
    let mode = resource_handler_mode();
    if let (ResourceHandlerMode::BuiltIn, Some(asset)) = (mode, asset) {
        // CefStreamReader::CreateForData does not own the source pointer. SchemeAsset
        // bytes are static and therefore remain alive beyond the stream handler.
        let stream =
            cef::stream_reader_create_for_data(asset.bytes.as_ptr().cast_mut(), asset.bytes.len());
        if let Some(stream) = stream {
            let (mime, _) = split_mime(asset.mime_type);
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-resource] request_id={request_id} handler=builtin length={} mime={mime:?} bytes_lifetime=static",
                    asset.bytes.len()
                );
            }
            let handler = StreamResourceHandler::new_with_stream(mime.to_string(), stream);
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[cef-ref] object_type=cef_resource_handler_t object_id={request_id} implementation=builtin event=return_to_cef has_one_ref={}",
                    handler.has_one_ref()
                );
            }
            return handler;
        }
        eprintln!(
            "[cef-resource] request_id={request_id} handler=builtin stream_create=false fallback=custom"
        );
    }

    if cef_diagnostics_enabled() {
        eprintln!(
            "[cef-resource] request_id={request_id} handler=custom length={} bytes_lifetime=static",
            asset.map(|asset| asset.bytes.len()).unwrap_or(0)
        );
    }
    let handler = PluginAssetHandler::new(
        asset,
        ReadState::new(request_id),
        ObjectLifetime::new("cef_resource_handler_t", request_id),
    );
    if cef_diagnostics_enabled() {
        eprintln!(
            "[cef-ref] object_type=cef_resource_handler_t object_id={request_id} implementation=custom event=return_to_cef has_one_ref={}",
            handler.has_one_ref()
        );
    }
    handler
}

/// One resolved asset. Bytes are `'static` because they live in a loaded
/// library's read-only data segment — nothing is copied to serve a request.
#[derive(Debug, Clone, Copy)]
pub struct SchemeAsset {
    pub bytes: &'static [u8],
    pub mime_type: &'static str,
}

/// Resolves `mikoplugin://<plugin>/<path>` to embedded bytes.
///
/// Returning `None` produces a 404. The resolver is responsible for rejecting
/// unknown plugin origins, which is what keeps one plugin's editor from reading
/// another's assets.
pub type SchemeResolver = Arc<dyn Fn(&str, &str) -> Option<SchemeAsset> + Send + Sync>;

/// Delivers one React->native bridge message: the raw JSON body POSTed to
/// `mikoplugin://<plugin>/__bridge`. Runs on CEF's IO thread like the
/// resolver — must not block, and is expected to just enqueue the bytes for a
/// later, non-realtime drain (see `builtin_plugin_editor::take_inbound`).
pub type BridgeSink = Arc<dyn Fn(&str, Vec<u8>) + Send + Sync>;

/// Receives CEF external-message-pump scheduling requests. CEF may invoke this
/// callback from any thread; the receiver is responsible for marshalling work
/// to the browser-process main thread.
pub type MessagePumpSchedule = Arc<dyn Fn(i64) + Send + Sync>;

wrap_browser_process_handler! {
    pub struct PluginBrowserProcessHandler {
        schedule: MessagePumpSchedule,
    }

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            (self.schedule)(delay_ms);
        }
    }
}

// Declares the plugin scheme in every CEF process. (`wrap_app!` matches on
// `$vis:vis struct`, so the description cannot be a doc comment here.)
wrap_app! {
    pub struct PluginSchemeApp {
        _lifetime: ObjectLifetime,
        browser_process_handler: Option<BrowserProcessHandler>,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut cef::CommandLine>,
        ) {
            if let Some(command_line) = command_line {
                // Built-in plugin editors have a fixed layout; browser pinch
                // zoom must never resize their document.
                for switch in local_ui_command_line_switches(std::env::consts::OS) {
                    command_line.append_switch(Some(&CefString::from(*switch)));
                }
                let disable_gpu = cef_gpu_disabled();
                let disable_compositing = cef_gpu_isolation_enabled();
                if disable_gpu {
                    log::info!(
                        "CEF GPU acceleration disabled by diagnostic/safe graphics mode compositing_disabled={disable_compositing}"
                    );
                }
                for (name, value) in extra_diagnostic_switches(
                    disable_gpu,
                    disable_compositing,
                    cef_diagnostics_enabled(),
                ) {
                    match value {
                        Some(value) => command_line.append_switch_with_value(
                            Some(&CefString::from(name)),
                            Some(&CefString::from(value)),
                        ),
                        None => command_line.append_switch(Some(&CefString::from(name))),
                    }
                }
            }
        }

        fn on_register_custom_schemes(&self, registrar: Option<&mut cef::SchemeRegistrar>) {
            let Some(registrar) = registrar else { return };
            // STANDARD gives the scheme real origin semantics (so the editor
            // gets a stable origin and relative URLs resolve). SECURE keeps it
            // out of Chromium's mixed-content and insecure-origin penalty box,
            // which otherwise blocks APIs the editor relies on. CORS_ENABLED
            // lets the document fetch its own sibling assets. FETCH_ENABLED is
            // separate from CORS_ENABLED and required for `fetch()` itself to
            // be allowed against this scheme at all — without it, the React
            // bridge's `fetch("__bridge", ...)` (its only way to reach native)
            // fails silently, native never sees `bridgeReady`, and the editor
            // is stuck showing its own empty state forever.
            let options = SchemeOptions::STANDARD.get_raw()
                | SchemeOptions::SECURE.get_raw()
                | SchemeOptions::CORS_ENABLED.get_raw()
                | SchemeOptions::FETCH_ENABLED.get_raw();
            let registered = registrar.add_custom_scheme(
                Some(&CefString::from(PLUGIN_SCHEME)),
                options as _,
            );
            if registered == 0 || cef_diagnostics_enabled() {
                eprintln!(
                    "[plugin-scheme] event=OnRegisterCustomSchemes scheme={PLUGIN_SCHEME} options={options} registered={} pid={} thread={:?}",
                    registered != 0,
                    std::process::id(),
                    std::thread::current().id()
                );
            }
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            self.browser_process_handler.clone()
        }
    }
}

// Creates one `ResourceHandler` per `mikoplugin://` request.
wrap_scheme_handler_factory! {
    pub struct PluginSchemeFactory {
        resolver: SchemeResolver,
        bridge: Option<BridgeSink>,
        _lifetime: ObjectLifetime,
    }

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut cef::Browser>,
            _frame: Option<&mut cef::Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut cef::Request>,
        ) -> Option<ResourceHandler> {
            let request = request?;
            // `url()` hands back a CEF-owned userfree string; copy it into an
            // owned Rust string before the borrow ends.
            let url = CefStringUtf16::from(&request.url()).to_string();
            let (plugin, path) = split_plugin_url(&url)?;

            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);

            if path == BRIDGE_PATH {
                let method = CefStringUtf16::from(&request.method()).to_string();
                if method.eq_ignore_ascii_case("POST") && cef_ipc_enabled() {
                    if let Some(sink) = &self.bridge {
                        let body = read_post_data(request);
                        if std::env::var_os("FUTUREBOARD_PLUGIN_VIEW_DEBUG").is_some() {
                            eprintln!(
                                "[plugin-bridge] inbound plugin={plugin} bytes={}",
                                body.len()
                            );
                        }
                        sink(&plugin, body);
                    }
                } else if method.eq_ignore_ascii_case("POST") && cef_diagnostics_enabled() {
                    eprintln!(
                        "[plugin-bridge] inbound dropped plugin={plugin} reason=ipc-disabled"
                    );
                }
                return Some(make_resource_handler(Some(BRIDGE_ACK), request_id));
            }

            if cef_static_test_enabled() {
                let asset = (path == "/index.html").then_some(STATIC_TEST_DOCUMENT);
                if cef_diagnostics_enabled() {
                    eprintln!(
                        "[plugin-scheme] request_id={request_id} url={url} static_test=true resolved={}",
                        asset.is_some()
                    );
                }
                return Some(make_resource_handler(asset, request_id));
            }

            // A miss still gets a handler, so the renderer sees a clean 404
            // instead of a failed-to-load network error.
            let asset = diagnostic_document(&path, (self.resolver)(&plugin, &path));
            if cef_diagnostics_enabled() {
                eprintln!(
                    "[plugin-scheme] request_id={request_id} url={url} plugin={plugin} path={path} resolved={} handler={:?} test_document={} length={}",
                    asset.is_some(),
                    resource_handler_mode(),
                    std::env::var("FUTUREBOARD_CEF_TEST_DOCUMENT").unwrap_or_else(|_| "current".to_string()),
                    asset.map(|asset| asset.bytes.len()).unwrap_or(0)
                );
            }
            Some(make_resource_handler(asset, request_id))
        }
    }
}

/// Concatenate every `PostDataElement`'s bytes. Bounded by whatever the page
/// actually POSTs (one JSON envelope) — never audio-rate, never large.
fn read_post_data(request: &mut cef::Request) -> Vec<u8> {
    let Some(post_data) = request.post_data() else {
        return Vec::new();
    };
    // `PostData::elements` uses the *current length* of the passed-in `Vec`
    // as the output buffer size (it does not grow it) — an empty vec means
    // "give me zero elements", not "give me however many there are". Must
    // pre-size to `element_count()` first.
    let count = post_data.element_count();
    if count == 0 {
        return Vec::new();
    }
    let mut elements: Vec<Option<cef::PostDataElement>> = (0..count).map(|_| None).collect();
    post_data.elements(Some(&mut elements));
    let mut out = Vec::new();
    for element in elements.into_iter().flatten() {
        let count = element.bytes_count();
        if count == 0 {
            continue;
        }
        let mut buf = vec![0u8; count];
        let read = element.bytes(count, buf.as_mut_ptr());
        buf.truncate(read);
        out.extend_from_slice(&buf);
    }
    out
}

#[derive(Clone)]
struct ReadState {
    request_id: u64,
    offset: Arc<Mutex<usize>>,
}

impl ReadState {
    fn new(request_id: u64) -> Self {
        Self {
            request_id,
            offset: Arc::new(Mutex::new(0)),
        }
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn read_sync(
        &self,
        method: &'static str,
        asset: Option<SchemeAsset>,
        data_out: *mut u8,
        bytes_to_read: ::std::os::raw::c_int,
        bytes_read: Option<&mut ::std::os::raw::c_int>,
    ) -> ::std::os::raw::c_int {
        let total = asset.map(|asset| asset.bytes.len()).unwrap_or(0);
        let mut offset = match self.offset.lock() {
            Ok(offset) => offset,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = *offset;
        let Some(bytes_read) = bytes_read else {
            self.log_read(method, before, total, bytes_to_read, 0, None, 0, before);
            return 0;
        };
        *bytes_read = 0;

        let Some(asset) = asset else {
            self.log_read(method, before, total, bytes_to_read, 0, Some(0), 0, before);
            return 0;
        };
        if data_out.is_null() || bytes_to_read <= 0 {
            self.log_read(method, before, total, bytes_to_read, 0, Some(0), 0, before);
            return 0;
        }

        let remaining = asset.bytes.len().saturating_sub(before);
        if remaining == 0 {
            // Synchronous EOF: false and exactly zero bytes. No callback is used.
            self.log_read(method, before, total, bytes_to_read, 0, Some(0), 0, before);
            return 0;
        }

        let copied = remaining.min(bytes_to_read as usize);
        // SAFETY: CEF supplies `data_out` with capacity `bytes_to_read`; copied
        // is bounded by that value and by the remaining static asset bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(asset.bytes[before..].as_ptr(), data_out, copied);
        }
        *offset = before + copied;
        *bytes_read = copied as ::std::os::raw::c_int;
        self.log_read(
            method,
            before,
            total,
            bytes_to_read,
            copied,
            Some(*bytes_read),
            1,
            *offset,
        );
        1
    }

    #[allow(clippy::too_many_arguments)]
    fn log_read(
        &self,
        method: &str,
        before: usize,
        total: usize,
        bytes_to_read: i32,
        copied: usize,
        bytes_read: Option<i32>,
        return_value: i32,
        after: usize,
    ) {
        if cef_diagnostics_enabled() {
            eprintln!(
                "[cef-resource] method={method} handler_id={} request_id={} offset_before={before} total_length={total} bytes_to_read={bytes_to_read} bytes_copied={copied} bytes_read_returned={bytes_read:?} return_value={return_value} offset_after={after} callback_invoked=false thread={:?}",
                self.request_id,
                self.request_id,
                std::thread::current().id()
            );
        }
    }

    fn skip_sync(
        &self,
        asset: Option<SchemeAsset>,
        bytes_to_skip: i64,
        bytes_skipped: Option<&mut i64>,
    ) -> ::std::os::raw::c_int {
        let mut offset = match self.offset.lock() {
            Ok(offset) => offset,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = *offset;
        let Some(bytes_skipped) = bytes_skipped else {
            return 0;
        };
        *bytes_skipped = 0;
        let Some(asset) = asset else { return 0 };
        if bytes_to_skip < 0 {
            return 0;
        }
        let skipped = asset
            .bytes
            .len()
            .saturating_sub(before)
            .min(bytes_to_skip as usize);
        *offset = before + skipped;
        *bytes_skipped = skipped as i64;
        if cef_diagnostics_enabled() {
            eprintln!(
                "[cef-resource] method=Skip handler_id={} request_id={} offset_before={before} total_length={} bytes_to_skip={bytes_to_skip} bytes_skipped={skipped} return_value=1 offset_after={} callback_invoked=false thread={:?}",
                self.request_id,
                self.request_id,
                asset.bytes.len(),
                *offset,
                std::thread::current().id()
            );
        }
        1
    }
}

// Serves one in-memory asset. Payload and cursor ownership are per request.
wrap_resource_handler! {
    pub struct PluginAssetHandler {
        asset: Option<SchemeAsset>,
        state: ReadState,
        _lifetime: ObjectLifetime,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut cef::Request>,
            handle_request: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut cef::Callback>,
        ) -> ::std::os::raw::c_int {
            if let Some(handle_request) = handle_request {
                *handle_request = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut cef::Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            let Some(response) = response else { return };
            match self.asset {
                Some(asset) => {
                    let (mime, charset) = split_mime(asset.mime_type);
                    response.set_status(200);
                    response.set_status_text(Some(&CefString::from("OK")));
                    response.set_mime_type(Some(&CefString::from(mime)));
                    if let Some(charset) = charset {
                        response.set_charset(Some(&CefString::from(charset)));
                    }
                    response.set_header_by_name(
                        Some(&CefString::from("Content-Type")),
                        Some(&CefString::from(asset.mime_type)),
                        1,
                    );
                    response.set_header_by_name(
                        Some(&CefString::from("Cache-Control")),
                        Some(&CefString::from("no-store")),
                        1,
                    );
                    if let Some(length) = response_length {
                        *length = asset.bytes.len() as i64;
                    }
                    if cef_diagnostics_enabled() {
                        let readback = CefStringUtf16::from(&response.mime_type()).to_string();
                        eprintln!(
                            "[plugin-scheme] request_id={} status=200 mime_set={mime} mime_readback={readback} length={} bytes_lifetime=static",
                            self.state.request_id,
                            asset.bytes.len()
                        );
                    }
                }
                None => {
                    response.set_status(404);
                    response.set_status_text(Some(&CefString::from("Not Found")));
                    response.set_mime_type(Some(&CefString::from("text/plain")));
                    if let Some(length) = response_length {
                        *length = 0;
                    }
                    if cef_diagnostics_enabled() {
                        eprintln!(
                            "[plugin-scheme] request_id={} status=404 length=0",
                            self.state.request_id
                        );
                    }
                }
            }
        }

        fn skip(
            &self,
            bytes_to_skip: i64,
            bytes_skipped: Option<&mut i64>,
            _callback: Option<&mut cef::ResourceSkipCallback>,
        ) -> ::std::os::raw::c_int {
            self.state.skip_sync(self.asset, bytes_to_skip, bytes_skipped)
        }

        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut cef::ResourceReadCallback>,
        ) -> ::std::os::raw::c_int {
            self.state
                .read_sync("Read", self.asset, data_out, bytes_to_read, bytes_read)
        }

        #[allow(clippy::not_unsafe_ptr_arg_deref)]
        fn read_response(
            &self,
            data_out: *mut u8,
            bytes_to_read: ::std::os::raw::c_int,
            bytes_read: Option<&mut ::std::os::raw::c_int>,
            _callback: Option<&mut cef::Callback>,
        ) -> ::std::os::raw::c_int {
            self.state.read_sync(
                "ReadResponse",
                self.asset,
                data_out,
                bytes_to_read,
                bytes_read,
            )
        }
    }
}

/// Split a `mime_type; charset=...` string into the bare MIME type and, when
/// present, the charset value. CEF's response mime-type field expects the
/// bare type; a charset embedded in it (e.g. `"text/html; charset=utf-8"`
/// stored as one token) is not parsed apart by the renderer and can leave it
/// unable to sniff the type as HTML.
fn split_mime(mime_type: &str) -> (&str, Option<&str>) {
    match mime_type.split_once(';') {
        Some((mime, rest)) => {
            let charset = rest.trim().strip_prefix("charset=").map(str::trim);
            (mime.trim(), charset)
        }
        None => (mime_type.trim(), None),
    }
}

/// Split `mikoplugin://<plugin>/<path>` into its origin and path.
///
/// Deliberately conservative: any `..` segment rejects the whole request, so a
/// traversal can never reach the resolver. An empty path maps to `/index.html`.
/// Returns `None` for a foreign scheme or an empty origin.
pub fn split_plugin_url(url: &str) -> Option<(String, String)> {
    let prefix = format!("{PLUGIN_SCHEME}://");
    if url.len() < prefix.len() || !url[..prefix.len()].eq_ignore_ascii_case(&prefix) {
        return None;
    }
    let rest = &url[prefix.len()..];

    let (host, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let host_end = host.find(['?', '#']).unwrap_or(host.len());
    let plugin = &host[..host_end];
    if plugin.is_empty() || plugin.contains('\\') || plugin == "." || plugin == ".." {
        return None;
    }

    let path_end = path.find(['?', '#']).unwrap_or(path.len());
    let mut segments: Vec<&str> = Vec::new();
    for segment in path[..path_end].split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            other => segments.push(other),
        }
    }
    let normalized = if segments.is_empty() {
        "/index.html".to_string()
    } else {
        let mut out = String::with_capacity(path_end + 1);
        for segment in &segments {
            out.push('/');
            out.push_str(segment);
        }
        out
    };

    Some((plugin.to_ascii_lowercase(), normalized))
}

/// Build the [`App`] that declares the plugin scheme. Pass the **same** app to
/// both `execute_subprocess` and `CefRuntime::initialize`.
pub fn plugin_scheme_app() -> Result<App, crate::runtime::CefRuntimeError> {
    // Constructing a `cef::App` is itself a CEF object creation, so macOS must
    // load the bundled framework and every platform must bind the API version
    // *here* — not when the app is consumed later. Skipping either step makes
    // the first generated CEF wrapper call invalid.
    crate::runtime::prepare_process()?;
    let object_id = NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed);
    let app = PluginSchemeApp::new(ObjectLifetime::new("cef_app_t", object_id), None);
    if cef_diagnostics_enabled() {
        eprintln!(
            "[cef-ref] object_type=cef_app_t object_id={object_id} event=return_to_caller has_one_ref={}",
            app.has_one_ref()
        );
    }
    Ok(app)
}

/// Build the plugin-scheme app with the pinned CEF external-pump callback.
///
/// The same object must still be passed to process dispatch and browser
/// initialization. This is exposed for the macOS integration probe; production
/// startup uses the integrated pump through [`plugin_scheme_app`].
pub fn plugin_scheme_app_with_message_pump(
    schedule: MessagePumpSchedule,
) -> Result<App, crate::runtime::CefRuntimeError> {
    crate::runtime::prepare_process()?;
    let object_id = NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed);
    Ok(PluginSchemeApp::new(
        ObjectLifetime::new("cef_app_t", object_id),
        Some(PluginBrowserProcessHandler::new(schedule)),
    ))
}

/// Install the handler that serves plugin assets. Call once, after
/// `CefRuntime::initialize` has returned successfully.
///
/// `domain_name` is `None` so the factory serves every origin under the scheme;
/// per-plugin isolation is enforced by the resolver, which only answers for
/// plugin ids it knows.
pub fn register_plugin_scheme_factory(
    resolver: SchemeResolver,
    bridge: Option<BridgeSink>,
) -> Result<(), SchemeError> {
    let object_id = NEXT_OBJECT_ID.fetch_add(1, Ordering::Relaxed);
    let mut factory = PluginSchemeFactory::new(
        resolver,
        bridge,
        ObjectLifetime::new("cef_scheme_handler_factory_t", object_id),
    );
    if cef_diagnostics_enabled() {
        eprintln!(
            "[cef-ref] object_type=cef_scheme_handler_factory_t object_id={object_id} event=before_register has_one_ref={}",
            factory.has_one_ref()
        );
    }
    let ok = cef::register_scheme_handler_factory(
        Some(&CefString::from(PLUGIN_SCHEME)),
        None,
        Some(&mut factory),
    );
    if cef_diagnostics_enabled() {
        eprintln!(
            "[cef-ref] object_type=cef_scheme_handler_factory_t object_id={object_id} event=after_register accepted={} has_one_ref={}",
            ok != 0,
            factory.has_one_ref()
        );
    }
    if ok == 0 {
        return Err(SchemeError::RegisterFactoryFailed);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum SchemeError {
    #[error("CEF rejected the {PLUGIN_SCHEME} scheme handler factory")]
    RegisterFactoryFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_keychain_switch_is_macos_only_and_has_no_cli_prefix() {
        let macos = local_ui_command_line_switches("macos");
        assert!(macos.contains(&MACOS_CEF_USE_MOCK_KEYCHAIN));
        assert!(!MACOS_CEF_USE_MOCK_KEYCHAIN.starts_with("--"));

        for target in ["windows", "linux"] {
            assert!(!local_ui_command_line_switches(target).contains(&MACOS_CEF_USE_MOCK_KEYCHAIN));
        }
    }

    #[test]
    fn gpu_isolation_adds_compositing_switch_without_replacing_legacy_gpu_off() {
        let legacy = extra_diagnostic_switches(true, false, false);
        assert_eq!(legacy, vec![(CEF_DISABLE_GPU, None)]);

        let isolation = extra_diagnostic_switches(true, true, false);
        assert!(isolation.contains(&(CEF_DISABLE_GPU, None)));
        assert!(isolation.contains(&("disable-gpu-compositing", None)));

        let verbose = extra_diagnostic_switches(false, false, true);
        assert!(verbose.contains(&("enable-logging", None)));
        assert!(verbose.contains(&("v", Some("1"))));
    }

    #[test]
    fn static_test_document_is_inert_html() {
        let text = std::str::from_utf8(STATIC_TEST_DOCUMENT.bytes).expect("utf-8");
        assert!(text.contains("Futureboard CEF Test"));
        assert!(!text.contains("<script"));
        assert!(!text.contains("mikoplugin"));
    }

    #[test]
    fn minimal_test_document_stays_below_one_hundred_bytes() {
        assert!(MINIMAL_TEST_DOCUMENT.bytes.len() < 100);
        assert_eq!(MINIMAL_TEST_DOCUMENT.mime_type, "text/html");
    }

    #[test]
    fn custom_reader_obeys_chunk_and_eof_contract() {
        let asset = SchemeAsset {
            bytes: b"abcdef",
            mime_type: "text/plain",
        };
        let state = ReadState::new(41);
        let mut first = [0u8; 4];
        let mut read = -1;
        assert_eq!(
            state.read_sync("test", Some(asset), first.as_mut_ptr(), 4, Some(&mut read)),
            1
        );
        assert_eq!(read, 4);
        assert_eq!(&first, b"abcd");

        let mut second = [0u8; 4];
        assert_eq!(
            state.read_sync("test", Some(asset), second.as_mut_ptr(), 4, Some(&mut read)),
            1
        );
        assert_eq!(read, 2);
        assert_eq!(&second[..2], b"ef");

        assert_eq!(
            state.read_sync("test", Some(asset), second.as_mut_ptr(), 4, Some(&mut read)),
            0
        );
        assert_eq!(read, 0);
    }

    #[test]
    fn each_custom_reader_has_independent_state() {
        let asset = SchemeAsset {
            bytes: b"abcd",
            mime_type: "text/plain",
        };
        let first = ReadState::new(1);
        let second = ReadState::new(2);
        let mut out = [0u8; 2];
        let mut read = 0;
        assert_eq!(
            first.read_sync("test", Some(asset), out.as_mut_ptr(), 2, Some(&mut read)),
            1
        );
        assert_eq!(
            second.read_sync("test", Some(asset), out.as_mut_ptr(), 2, Some(&mut read)),
            1
        );
        assert_eq!(&out, b"ab");
    }

    #[test]
    fn splits_mime_and_charset() {
        assert_eq!(
            split_mime("text/html; charset=utf-8"),
            ("text/html", Some("utf-8"))
        );
        assert_eq!(split_mime("application/wasm"), ("application/wasm", None));
        assert_eq!(
            split_mime("text/javascript;charset=utf-8"),
            ("text/javascript", Some("utf-8"))
        );
    }

    #[test]
    fn parses_plugin_and_path() {
        assert_eq!(
            split_plugin_url("mikoplugin://rodharerist/index.html"),
            Some(("rodharerist".into(), "/index.html".into()))
        );
        assert_eq!(
            split_plugin_url("mikoplugin://rodharerist/assets/app-A1b2.js"),
            Some(("rodharerist".into(), "/assets/app-A1b2.js".into()))
        );
    }

    #[test]
    fn bare_origin_and_root_map_to_index() {
        for url in [
            "mikoplugin://rodharerist",
            "mikoplugin://rodharerist/",
            "mikoplugin://rodharerist/?v=2",
        ] {
            assert_eq!(
                split_plugin_url(url),
                Some(("rodharerist".into(), "/index.html".into())),
                "{url}"
            );
        }
    }

    #[test]
    fn scheme_match_is_case_insensitive_and_origin_is_normalized() {
        assert_eq!(
            split_plugin_url("MikoPlugin://Rodhareist/index.html"),
            Some(("rodhareist".into(), "/index.html".into()))
        );
    }

    #[test]
    fn query_and_fragment_are_stripped_from_the_path() {
        assert_eq!(
            split_plugin_url("mikoplugin://rod/a.js?v=1#x"),
            Some(("rod".into(), "/a.js".into()))
        );
    }

    #[test]
    fn traversal_is_rejected_before_reaching_the_resolver() {
        for url in [
            "mikoplugin://rodharerist/../secret",
            "mikoplugin://rodharerist/assets/../../secret",
            "mikoplugin://../index.html",
            "mikoplugin://../../index.html",
        ] {
            assert_eq!(split_plugin_url(url), None, "{url}");
        }
    }

    #[test]
    fn foreign_schemes_and_empty_origins_are_rejected() {
        assert_eq!(split_plugin_url("https://example.com/index.html"), None);
        assert_eq!(split_plugin_url("mikoplugin:///index.html"), None);
        assert_eq!(split_plugin_url("mikoplugin://"), None);
        assert_eq!(split_plugin_url(""), None);
    }

    #[test]
    fn never_panics_on_malformed_input() {
        for url in [
            "mikoplugin",
            "mikoplugin:",
            "mikoplugin:/",
            "mikoplugin://%",
            "mikoplugin://a/%zz",
            "mikoplugin://a\\b/c",
        ] {
            let _ = split_plugin_url(url);
        }
    }
}
