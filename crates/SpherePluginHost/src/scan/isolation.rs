use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::scan::log::{
    scan_finished, scan_found, scan_plugin_failed, scan_plugin_start, scan_plugin_success,
    scan_process_crashed, scan_start,
};
use crate::scan::types::{
    PluginDescriptor, PluginScanError, PluginScanFormat, PluginScanStatus, ScanFailureRecord,
    ScanResultPayload,
};
use crate::scanner::{scan_clap_paths, scan_vst2_paths, scan_vst3_paths};
use crate::types::PluginInfo;

/// Marker the scanner child prints immediately before its JSON payload.
///
/// Plug-in modules write to the child's stdout while they load — Kontakt 8
/// prints two `[info]` log lines — and that text landed in front of the payload,
/// so `serde_json` rejected the whole scan and every class in the bundle was
/// lost. The child now isolates its payload behind this marker (and redirects
/// stray writes to stderr); the parent takes the text after the last marker.
pub const SCAN_PAYLOAD_SENTINEL: &str = "@@FUTUREBOARD_SCAN_PAYLOAD@@";

/// Wall-clock budget for scanning one plug-in bundle. Loading a module runs
/// vendor code that can block forever: `soothe3`, `Clear`, and two Neural DSP
/// bundles on the reference machine never return, which used to wedge the scan
/// thread permanently — no catalog was ever written, so *no* plug-in was
/// discovered. The slowest bundle that does complete takes ~11 s, so 30 s is
/// generous while still bounded. Override with
/// `FUTUREBOARD_PLUGIN_SCAN_TIMEOUT_SECS`.
const DEFAULT_BUNDLE_SCAN_TIMEOUT_SECS: u64 = 30;

/// Budget for a whole-format scan (AudioUnit enumeration, or a folder sweep in
/// builds without per-bundle isolation). Larger because it covers every plug-in
/// in one child, but still finite.
const DEFAULT_FORMAT_SCAN_TIMEOUT_SECS: u64 = 600;

fn timeout_from_env(var: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

fn bundle_scan_timeout() -> Duration {
    timeout_from_env(
        "FUTUREBOARD_PLUGIN_SCAN_TIMEOUT_SECS",
        DEFAULT_BUNDLE_SCAN_TIMEOUT_SECS,
    )
}

fn format_scan_timeout() -> Duration {
    timeout_from_env(
        "FUTUREBOARD_PLUGIN_FORMAT_SCAN_TIMEOUT_SECS",
        DEFAULT_FORMAT_SCAN_TIMEOUT_SECS,
    )
}

/// Everything a finished (or killed) scanner child produced.
struct ProcessCapture {
    /// `None` when the child was killed after exceeding its budget.
    status: Option<ExitStatus>,
    stdout: String,
    stderr: String,
}

impl ProcessCapture {
    fn timed_out(&self) -> bool {
        self.status.is_none()
    }

    fn success(&self) -> bool {
        self.status.is_some_and(|status| status.success())
    }

    fn code(&self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }
}

/// Run a scanner child to completion or `timeout`, whichever comes first.
///
/// Both pipes are drained on their own threads. `Child::wait_with_output` is not
/// usable here because it cannot be given a deadline, and polling `try_wait`
/// without draining would deadlock the child as soon as its output exceeds the
/// pipe buffer — one Waves shell alone emits 718 classes.
///
/// Scanner/offline path only: the poll sleep is never reached from an audio
/// callback.
fn run_scanner_process(
    command: &mut Command,
    timeout: Duration,
) -> std::io::Result<ProcessCapture> {
    let mut child: Child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout_reader = PipeDrain::spawn(child.stdout.take());
    let stderr_reader = PipeDrain::spawn(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };

    // The child is gone, but its pipes only reach EOF once every process
    // holding their write ends has closed them — and a plug-in that started a
    // resident helper (a licensing daemon) while it loaded may have handed
    // them down to it. Waiting for EOF then ignored the timeout and stalled
    // the whole sweep for as long as the helper lived, so the drain gets a
    // short grace and then keeps whatever has arrived.
    let drain_deadline = Instant::now() + PIPE_DRAIN_GRACE;
    Ok(ProcessCapture {
        status,
        stdout: stdout_reader.finish(drain_deadline),
        stderr: stderr_reader.finish(drain_deadline),
    })
}

/// How long a scanner child's pipes may stay open after the child has exited
/// before the parent stops waiting for EOF. Output still in flight when the
/// child exits arrives well within this; only a pipe held open by some other
/// process (a helper the plug-in started) outlasts it.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// One child pipe drained on its own thread into a shared buffer, so the
/// parent can take what has arrived without waiting for EOF.
struct PipeDrain {
    buffer: std::sync::Arc<parking_lot::Mutex<Vec<u8>>>,
    eof: std::sync::mpsc::Receiver<()>,
}

impl PipeDrain {
    fn spawn<R: Read + Send + 'static>(pipe: Option<R>) -> Self {
        let buffer = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (eof_tx, eof) = std::sync::mpsc::channel();
        if let Some(mut pipe) = pipe {
            let sink = buffer.clone();
            // Detached on purpose: if a helper process keeps the pipe open,
            // this thread stays parked in `read` until the helper exits and
            // must not hold the scan up meanwhile.
            let _ = std::thread::Builder::new()
                .name("plugin-scan-pipe".into())
                .spawn(move || {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match pipe.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(read) => sink.lock().extend_from_slice(&chunk[..read]),
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                    let _ = eof_tx.send(());
                });
        } else {
            let _ = eof_tx.send(());
        }
        Self { buffer, eof }
    }

    /// Wait for EOF until `deadline`, then return everything read so far.
    fn finish(self, deadline: Instant) -> String {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Disconnected (the thread could not start) is as final as EOF.
        let _ = self.eof.recv_timeout(remaining);
        let bytes = std::mem::take(&mut *self.buffer.lock());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Pull the JSON payload out of a scanner child's stdout.
///
/// Prefers the text after the last [`SCAN_PAYLOAD_SENTINEL`]. Falls back to the
/// first `{` so a payload written by an older scanner binary (or one whose
/// marker was swallowed) is still recoverable instead of failing the bundle.
pub fn extract_payload_json(stdout: &str) -> Option<&str> {
    if let Some(marker) = stdout.rfind(SCAN_PAYLOAD_SENTINEL) {
        let rest = &stdout[marker + SCAN_PAYLOAD_SENTINEL.len()..];
        let end = rest.find('\n').unwrap_or(rest.len());
        let payload = rest[..end].trim();
        if !payload.is_empty() {
            return Some(payload);
        }
    }
    let start = stdout.find('{')?;
    let payload = stdout[start..].trim_end();
    (!payload.is_empty()).then_some(payload)
}

#[derive(Debug, Clone)]
pub struct IsolatedScanRequest {
    pub format: PluginScanFormat,
    pub paths: Vec<PathBuf>,
    pub validate_plugins: bool,
}

/// Result of scanning one plug-in bundle: the classes that produced real
/// metadata, plus the modules that did not. Failures used to be dropped, and the
/// unreadable modules were emitted as if they were plug-ins.
#[derive(Debug, Clone, Default)]
pub struct BundleScanOutcome {
    pub plugins: Vec<PluginInfo>,
    pub failures: Vec<ScanFailureRecord>,
}

#[derive(Debug, Clone)]
pub struct IsolatedScanOutcome {
    pub payload: ScanResultPayload,
    pub error: Option<PluginScanError>,
}

#[derive(Debug, Clone)]
pub enum ScannerBinaryLocation {
    EnvOverride(PathBuf),
    AdjacentToCurrentExe(PathBuf),
    CompileTime(PathBuf),
}

pub fn locate_scanner_binary() -> Result<PathBuf, PluginScanError> {
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(dir) = current_exe.parent() {
            let candidate = scanner_binary_name(dir);
            if candidate.is_file() {
                return Ok(candidate);
            }
            return Err(PluginScanError::ScannerBinaryMissing(
                candidate.display().to_string(),
            ));
        }
    }

    Err(PluginScanError::ScannerBinaryMissing(
        "{appdir}/FutureboardPluginScanner.exe".into(),
    ))
}

pub fn run_isolated_bundle_scan(bundle: &Path) -> Result<BundleScanOutcome, String> {
    let format = bundle_scan_format(bundle)
        .ok_or_else(|| format!("Unsupported plug-in bundle: {}", bundle.display()))?;
    let scanner = locate_scanner_binary().map_err(|error| error.message())?;
    let mut command = Command::new(&scanner);
    command
        .arg("--format")
        .arg(format.cli_arg())
        .arg("--json")
        .arg("--path")
        .arg(bundle);

    let timeout = bundle_scan_timeout();
    let capture = run_scanner_process(&mut command, timeout)
        .map_err(|error| PluginScanError::ScannerLaunchFailed(error.to_string()).message())?;

    if capture.timed_out() {
        // The child was killed, so the sweep moves on to the next bundle instead
        // of hanging the whole scan on one plug-in.
        let error = PluginScanError::ScannerTimedOut {
            format,
            seconds: timeout.as_secs(),
        };
        scan_plugin_failed(format, &bundle.display().to_string(), &error.message());
        return Err(error.message());
    }

    if !capture.success() {
        let exit_code = capture.code();
        let detail = capture.stderr.trim();
        return Err(match (exit_code, detail.is_empty()) {
            (Some(code), true) => {
                format!("{} scanner process crashed (exit {code})", format.cli_arg())
            }
            (Some(code), false) => {
                format!(
                    "{} scanner process failed (exit {code}): {detail}",
                    format.cli_arg()
                )
            }
            (None, true) => format!("{} scanner process crashed", format.cli_arg()),
            (None, false) => format!("{} scanner process crashed: {detail}", format.cli_arg()),
        });
    }

    let json = extract_payload_json(&capture.stdout).ok_or_else(|| {
        PluginScanError::ScannerOutputInvalid("scanner produced no JSON payload".into()).message()
    })?;
    let payload: ScanResultPayload = serde_json::from_str(json)
        .map_err(|error| PluginScanError::ScannerOutputInvalid(error.to_string()).message())?;
    if payload.process_crashed {
        return Err(payload
            .error
            .unwrap_or_else(|| format!("{} scanner process crashed", format.cli_arg())));
    }
    if let Some(error) = payload.error {
        return Err(error);
    }
    Ok(partition_bundle_payload(payload))
}

/// Split a scanner payload into usable plug-ins and reportable failures.
///
/// A module the scanner could not open still comes back as a descriptor so its
/// path and error survive, but it is not a plug-in: listing it put phantom rows
/// (name taken from the filename, category "Uncategorized") in the browser that
/// failed the moment they were inserted.
fn partition_bundle_payload(payload: ScanResultPayload) -> BundleScanOutcome {
    let mut outcome = BundleScanOutcome {
        failures: payload.failures,
        ..BundleScanOutcome::default()
    };
    for descriptor in &payload.plugins {
        if descriptor.sdk_metadata_loaded {
            outcome
                .plugins
                .push(plugin_info_from_descriptor(descriptor));
        } else {
            outcome.failures.push(ScanFailureRecord {
                path: descriptor.path_or_identifier.clone(),
                error: descriptor
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Plug-in module could not be loaded".to_string()),
                scan_status: PluginScanStatus::Failed,
            });
        }
    }
    outcome
}

fn bundle_scan_format(bundle: &Path) -> Option<PluginScanFormat> {
    let ext = bundle.extension()?.to_str()?;
    match ext.to_ascii_lowercase().as_str() {
        "vst3" => Some(PluginScanFormat::Vst3),
        "clap" => Some(PluginScanFormat::Clap),
        // VST2: a `.vst` bundle on macOS, a bare `.dll` on Windows. A `.dll`
        // candidate is not necessarily a plug-in — the native scanner probes it
        // for a VST2 entry point and returns nothing when it is just a support
        // library sitting in the same folder.
        "vst" | "vst2" | "dll" => Some(PluginScanFormat::Vst2),
        _ => None,
    }
}

fn scanner_binary_name(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        dir.join("FutureboardPluginScanner.exe")
    }
    #[cfg(not(windows))]
    {
        dir.join("FutureboardPluginScanner")
    }
}

pub fn run_isolated_format_scan(request: IsolatedScanRequest) -> IsolatedScanOutcome {
    scan_start(request.format);

    if !request.format.available_on_current_platform() {
        let error = PluginScanError::UnsupportedPlatform;
        return IsolatedScanOutcome {
            payload: ScanResultPayload {
                format: request.format,
                success: true,
                plugins: Vec::new(),
                failures: Vec::new(),
                crashed_plugins: Vec::new(),
                process_crashed: false,
                exit_code: None,
                error: Some(error.message()),
                scanned_paths: request.paths,
            },
            error: Some(error),
        };
    }

    // Prefer the out-of-process scanner for EVERY format, not just AudioUnit, so
    // a crashing or malicious plugin takes down the scanner child rather than the
    // host process. (Previously VST3/CLAP loaded plugin binaries directly
    // in-process here — `catch_unwind` cannot stop a C++ access violation, so a
    // single bad plugin crashed the app despite the "isolated" name.) The
    // in-process branch below is a best-effort fallback for builds shipped
    // without the scanner binary.
    if locate_scanner_binary().is_ok() {
        return run_subprocess_scan(request);
    }

    if request.format == PluginScanFormat::AudioUnit {
        return run_inprocess_au_scan(request);
    }

    match run_inprocess_scan(request.format, &request.paths) {
        Ok(payload) => {
            scan_found(request.format, payload.plugins.len());
            scan_finished(
                request.format,
                payload.plugins.len(),
                payload.failures.len(),
                payload.crashed_plugins.len(),
            );
            IsolatedScanOutcome {
                payload,
                error: None,
            }
        }
        Err(error) => {
            scan_finished(request.format, 0, 1, 0);
            IsolatedScanOutcome {
                payload: ScanResultPayload {
                    format: request.format,
                    success: false,
                    plugins: Vec::new(),
                    failures: request
                        .paths
                        .iter()
                        .map(|path| ScanFailureRecord {
                            path: path.display().to_string(),
                            error: error.message(),
                            scan_status: PluginScanStatus::Failed,
                        })
                        .collect(),
                    crashed_plugins: Vec::new(),
                    process_crashed: false,
                    exit_code: None,
                    error: Some(error.message()),
                    scanned_paths: request.paths,
                },
                error: Some(error),
            }
        }
    }
}

pub fn run_isolated_plugin_validation(
    format: PluginScanFormat,
    component_id: &str,
) -> Result<bool, PluginScanError> {
    if format != PluginScanFormat::AudioUnit {
        return Ok(true);
    }
    if !cfg!(target_os = "macos") {
        return Err(PluginScanError::UnsupportedPlatform);
    }

    let scanner = locate_scanner_binary();
    if scanner.is_ok() {
        let scanner = scanner?;
        let mut command = Command::new(&scanner);
        command
            .arg("--format")
            .arg(format.cli_arg())
            .arg("--json")
            .arg("--validate")
            .arg(component_id);
        let capture = run_scanner_process(&mut command, bundle_scan_timeout())
            .map_err(|error| PluginScanError::ScannerLaunchFailed(error.to_string()))?;
        if !capture.success() {
            return Ok(false);
        }
        return Ok(capture.stdout.contains("\"ok\":true"));
    }

    crate::au_scanner::validate_au_component(component_id)
}

fn run_subprocess_scan(request: IsolatedScanRequest) -> IsolatedScanOutcome {
    let scanner = match locate_scanner_binary() {
        Ok(path) => path,
        Err(error) => {
            if request.format == PluginScanFormat::AudioUnit {
                return run_inprocess_au_scan(request);
            }
            return IsolatedScanOutcome {
                payload: ScanResultPayload {
                    format: request.format,
                    success: false,
                    plugins: Vec::new(),
                    failures: Vec::new(),
                    crashed_plugins: Vec::new(),
                    process_crashed: false,
                    exit_code: None,
                    error: Some(error.message()),
                    scanned_paths: request.paths,
                },
                error: Some(error),
            };
        }
    };

    let mut command = Command::new(&scanner);
    command
        .arg("--format")
        .arg(request.format.cli_arg())
        .arg("--json");
    if request.validate_plugins {
        command.arg("--validate-plugins");
    }
    for path in &request.paths {
        command.arg("--path").arg(path);
    }

    let timeout = format_scan_timeout();
    let capture = match run_scanner_process(&mut command, timeout) {
        Ok(capture) => capture,
        Err(error) => {
            let scan_error = PluginScanError::ScannerLaunchFailed(error.to_string());
            return IsolatedScanOutcome {
                payload: ScanResultPayload {
                    format: request.format,
                    success: false,
                    plugins: Vec::new(),
                    failures: Vec::new(),
                    crashed_plugins: Vec::new(),
                    process_crashed: false,
                    exit_code: None,
                    error: Some(scan_error.message()),
                    scanned_paths: request.paths,
                },
                error: Some(scan_error),
            };
        }
    };

    if capture.timed_out() {
        let scan_error = PluginScanError::ScannerTimedOut {
            format: request.format,
            seconds: timeout.as_secs(),
        };
        scan_finished(request.format, 0, 1, 0);
        return IsolatedScanOutcome {
            payload: ScanResultPayload {
                format: request.format,
                success: false,
                plugins: Vec::new(),
                failures: request
                    .paths
                    .iter()
                    .map(|path| ScanFailureRecord {
                        path: path.display().to_string(),
                        error: scan_error.message(),
                        scan_status: PluginScanStatus::Failed,
                    })
                    .collect(),
                crashed_plugins: Vec::new(),
                process_crashed: false,
                exit_code: None,
                error: Some(scan_error.message()),
                scanned_paths: request.paths,
            },
            error: Some(scan_error),
        };
    }

    let exit_code = capture.code();
    if !capture.success() {
        scan_process_crashed(request.format, exit_code);
        let error = PluginScanError::ScannerProcessCrashed {
            format: request.format,
            exit_code,
        };
        scan_finished(request.format, 0, 0, 1);
        return IsolatedScanOutcome {
            payload: ScanResultPayload::process_crash(request.format, exit_code, error.message()),
            error: Some(error),
        };
    }

    let stdout = extract_payload_json(&capture.stdout).unwrap_or("");
    match serde_json::from_str::<ScanResultPayload>(stdout) {
        Ok(mut payload) => {
            if payload.scanned_paths.is_empty() {
                payload.scanned_paths = request.paths;
            }
            scan_found(request.format, payload.plugins.len());
            scan_finished(
                request.format,
                payload.plugins.len(),
                payload.failures.len(),
                payload.crashed_plugins.len(),
            );
            IsolatedScanOutcome {
                payload,
                error: None,
            }
        }
        Err(error) => {
            let scan_error =
                PluginScanError::ScannerOutputInvalid(format!("{error}; stdout={stdout}"));
            IsolatedScanOutcome {
                payload: ScanResultPayload {
                    format: request.format,
                    success: false,
                    plugins: Vec::new(),
                    failures: Vec::new(),
                    crashed_plugins: Vec::new(),
                    process_crashed: false,
                    exit_code,
                    error: Some(scan_error.message()),
                    scanned_paths: request.paths,
                },
                error: Some(scan_error),
            }
        }
    }
}

fn run_inprocess_au_scan(request: IsolatedScanRequest) -> IsolatedScanOutcome {
    match crate::au_scanner::scan_audio_units(request.validate_plugins) {
        Ok(plugins) => {
            scan_found(request.format, plugins.len());
            scan_finished(request.format, plugins.len(), 0, 0);
            IsolatedScanOutcome {
                payload: ScanResultPayload {
                    format: request.format,
                    success: true,
                    plugins,
                    failures: Vec::new(),
                    crashed_plugins: Vec::new(),
                    process_crashed: false,
                    exit_code: Some(0),
                    error: None,
                    scanned_paths: request.paths,
                },
                error: None,
            }
        }
        Err(error) => {
            scan_finished(request.format, 0, 1, 0);
            IsolatedScanOutcome {
                payload: ScanResultPayload {
                    format: request.format,
                    success: false,
                    plugins: Vec::new(),
                    failures: Vec::new(),
                    crashed_plugins: Vec::new(),
                    process_crashed: false,
                    exit_code: None,
                    error: Some(error.message()),
                    scanned_paths: request.paths,
                },
                error: Some(error),
            }
        }
    }
}

fn run_inprocess_scan(
    format: PluginScanFormat,
    paths: &[PathBuf],
) -> Result<ScanResultPayload, PluginScanError> {
    let path_strings: Vec<String> = paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let infos = match format {
        PluginScanFormat::Vst3 => scan_vst3_paths(&path_strings),
        PluginScanFormat::Vst2 => scan_vst2_paths(&path_strings),
        PluginScanFormat::Clap => scan_clap_paths(&path_strings),
        PluginScanFormat::AudioUnit => {
            return Err(PluginScanError::AudioUnitUnavailable);
        }
    }
    .map_err(PluginScanError::NativeScanFailed)?;

    // Modules whose metadata could not be read are failures, not plug-ins. They
    // used to be emitted as ordinary rows with a filename-derived name, which is
    // both a phantom entry in the browser and the only place classification ever
    // saw a filename.
    let mut plugins = Vec::with_capacity(infos.len());
    let mut failures = Vec::new();
    for info in infos {
        if info.sdk_metadata_loaded {
            plugins.push(plugin_descriptor_from_info(info));
        } else {
            failures.push(ScanFailureRecord {
                path: info.path.clone(),
                error: info
                    .load_error
                    .clone()
                    .unwrap_or_else(|| "Plug-in module could not be loaded".to_string()),
                scan_status: PluginScanStatus::Failed,
            });
        }
    }
    Ok(ScanResultPayload {
        format,
        success: true,
        plugins,
        failures,
        crashed_plugins: Vec::new(),
        process_crashed: false,
        exit_code: Some(0),
        error: None,
        scanned_paths: paths.to_vec(),
    })
}

pub fn plugin_descriptor_from_info(info: PluginInfo) -> PluginDescriptor {
    let format = info.format.to_ascii_uppercase();
    // Same declared-metadata rule the registry uses, so the descriptor a scanner
    // child emits and the row the app builds always agree. `is_instrument` and
    // `is_effect` are both false for an undeclared plug-in.
    let kind = crate::registry::classify_kind(
        crate::registry::PluginFormat::from_str_lossy(&format),
        &info.category,
        info.sub_categories.as_deref(),
        info.sdk_metadata_loaded,
    );
    let is_instrument = kind == crate::registry::PluginKind::Instrument;
    let is_effect = kind == crate::registry::PluginKind::Effect;
    PluginDescriptor {
        id: info.id,
        format,
        name: info.name.clone(),
        vendor: info.vendor,
        version: info.version,
        path_or_identifier: info.path,
        category: info.category,
        is_instrument,
        is_effect,
        scan_status: if info.sdk_metadata_loaded {
            PluginScanStatus::Success
        } else {
            PluginScanStatus::Failed
        },
        error_message: info.load_error,
        class_id: info.class_id,
        sub_categories: info.sub_categories,
        is_ara: info.is_ara,
        sdk_metadata_loaded: info.sdk_metadata_loaded,
    }
}

pub fn plugin_info_from_descriptor(descriptor: &PluginDescriptor) -> PluginInfo {
    PluginInfo {
        id: descriptor.id.clone(),
        name: descriptor.name.clone(),
        vendor: descriptor.vendor.clone(),
        category: descriptor.category.clone(),
        sub_categories: descriptor.sub_categories.clone(),
        format: descriptor.format.clone(),
        path: descriptor.path_or_identifier.clone(),
        module_path: Some(descriptor.path_or_identifier.clone()),
        class_id: descriptor.class_id.clone(),
        version: descriptor.version.clone(),
        sdk_version: None,
        is_shell_child: false,
        is_ara: descriptor.is_ara,
        sdk_metadata_loaded: descriptor.sdk_metadata_loaded,
        load_error: descriptor.error_message.clone(),
    }
}

pub fn run_direct_format_scan_for_cli(
    format: PluginScanFormat,
    paths: &[PathBuf],
    validate_plugins: bool,
) -> ScanResultPayload {
    if format == PluginScanFormat::AudioUnit {
        return run_direct_au_scan_for_cli(validate_plugins);
    }

    match run_inprocess_scan(format, paths) {
        Ok(payload) => payload,
        Err(error) => ScanResultPayload {
            format,
            success: false,
            plugins: Vec::new(),
            failures: paths
                .iter()
                .map(|path| ScanFailureRecord {
                    path: path.display().to_string(),
                    error: error.message(),
                    scan_status: PluginScanStatus::Failed,
                })
                .collect(),
            crashed_plugins: Vec::new(),
            process_crashed: false,
            exit_code: None,
            error: Some(error.message()),
            scanned_paths: paths.to_vec(),
        },
    }
}

fn run_direct_au_scan_for_cli(validate_plugins: bool) -> ScanResultPayload {
    if !cfg!(target_os = "macos") {
        return ScanResultPayload {
            format: PluginScanFormat::AudioUnit,
            success: true,
            plugins: Vec::new(),
            failures: Vec::new(),
            crashed_plugins: Vec::new(),
            process_crashed: false,
            exit_code: Some(0),
            error: Some(PluginScanError::UnsupportedPlatform.message()),
            scanned_paths: Vec::new(),
        };
    }

    let enumerated = match crate::au_scanner::scan_audio_units(false) {
        Ok(plugins) => plugins,
        Err(error) => {
            return ScanResultPayload {
                format: PluginScanFormat::AudioUnit,
                success: false,
                plugins: Vec::new(),
                failures: Vec::new(),
                crashed_plugins: Vec::new(),
                process_crashed: false,
                exit_code: None,
                error: Some(error.message()),
                scanned_paths: Vec::new(),
            };
        }
    };

    if !validate_plugins {
        scan_found(PluginScanFormat::AudioUnit, enumerated.len());
        scan_finished(PluginScanFormat::AudioUnit, enumerated.len(), 0, 0);
        return ScanResultPayload {
            format: PluginScanFormat::AudioUnit,
            success: true,
            plugins: enumerated,
            failures: Vec::new(),
            crashed_plugins: Vec::new(),
            process_crashed: false,
            exit_code: Some(0),
            error: None,
            scanned_paths: Vec::new(),
        };
    }

    let mut validated = Vec::new();
    let mut failures = Vec::new();
    let mut crashed = Vec::new();

    for plugin in enumerated {
        let identifier = plugin
            .class_id
            .clone()
            .unwrap_or_else(|| plugin.path_or_identifier.clone());
        scan_plugin_start(PluginScanFormat::AudioUnit, &identifier);

        match validate_au_in_child(&identifier) {
            Ok(true) => {
                scan_plugin_success(PluginScanFormat::AudioUnit, &identifier);
                validated.push(plugin);
            }
            Ok(false) => {
                scan_plugin_failed(
                    PluginScanFormat::AudioUnit,
                    &identifier,
                    "validation failed",
                );
                failures.push(ScanFailureRecord {
                    path: identifier.clone(),
                    error: PluginScanError::AudioUnitInstantiationFailed(
                        "validation failed".into(),
                    )
                    .message(),
                    scan_status: PluginScanStatus::Failed,
                });
            }
            Err(PluginScanError::AudioUnitScannerCrashed { exit_code }) => {
                scan_process_crashed(PluginScanFormat::AudioUnit, exit_code);
                crashed.push(ScanFailureRecord {
                    path: identifier,
                    error: PluginScanError::AudioUnitScannerCrashed { exit_code }.message(),
                    scan_status: PluginScanStatus::Crashed,
                });
            }
            Err(error) => {
                scan_plugin_failed(PluginScanFormat::AudioUnit, &identifier, &error.message());
                failures.push(ScanFailureRecord {
                    path: identifier,
                    error: error.message(),
                    scan_status: PluginScanStatus::Failed,
                });
            }
        }
    }

    scan_finished(
        PluginScanFormat::AudioUnit,
        validated.len(),
        failures.len(),
        crashed.len(),
    );

    ScanResultPayload {
        format: PluginScanFormat::AudioUnit,
        success: crashed.is_empty(),
        plugins: validated,
        failures,
        crashed_plugins: crashed,
        process_crashed: false,
        exit_code: Some(0),
        error: None,
        scanned_paths: Vec::new(),
    }
}

#[cfg(test)]
mod payload_tests {
    use super::{extract_payload_json, SCAN_PAYLOAD_SENTINEL};

    #[test]
    fn payload_survives_plugin_chatter_on_stdout() {
        // Kontakt 8 prints two timestamped log lines while its module loads;
        // parsing the whole of stdout rejected the scan and lost every class in
        // the bundle.
        let stdout = format!(
            "[2026-08-09 02:21:38.017] [info] initializing...\n\
             {SCAN_PAYLOAD_SENTINEL}{{\"format\":\"vst3\"}}\n"
        );
        assert_eq!(extract_payload_json(&stdout), Some("{\"format\":\"vst3\"}"));
    }

    #[test]
    fn payload_survives_chatter_printed_after_it() {
        let stdout =
            format!("{SCAN_PAYLOAD_SENTINEL}{{\"format\":\"vst3\"}}\n[info] shutting down\n");
        assert_eq!(extract_payload_json(&stdout), Some("{\"format\":\"vst3\"}"));
    }

    #[test]
    fn unmarked_payload_from_an_older_scanner_is_still_read() {
        assert_eq!(
            extract_payload_json("{\"format\":\"vst3\"}\n"),
            Some("{\"format\":\"vst3\"}")
        );
    }

    #[test]
    fn output_with_no_payload_is_rejected() {
        assert_eq!(extract_payload_json(""), None);
        assert_eq!(extract_payload_json("[info] only chatter\n"), None);
    }
}

#[cfg(test)]
mod pipe_drain_tests {
    use super::{run_scanner_process, PipeDrain};
    use std::io::Read;
    use std::time::{Duration, Instant};

    /// A pipe whose write end some other process still holds: it delivers what
    /// was written, then never reaches EOF.
    struct HeldOpenPipe {
        sent: bool,
        _never: std::sync::mpsc::Receiver<()>,
        _keep: std::sync::mpsc::Sender<()>,
    }

    impl Read for HeldOpenPipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let payload = b"payload";
                buf[..payload.len()].copy_from_slice(payload);
                return Ok(payload.len());
            }
            // Blocks forever: the sender is alive in `_keep`.
            let _ = self._never.recv();
            Ok(0)
        }
    }

    #[test]
    fn a_pipe_that_never_closes_still_yields_its_output_by_the_deadline() {
        let (keep, never) = std::sync::mpsc::channel();
        let drain = PipeDrain::spawn(Some(HeldOpenPipe {
            sent: false,
            _never: never,
            _keep: keep,
        }));
        let started = Instant::now();
        let output = drain.finish(Instant::now() + Duration::from_millis(300));
        assert_eq!(output, "payload");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "finish waited for EOF instead of its deadline"
        );
    }

    #[test]
    fn a_closed_pipe_returns_everything_without_waiting_out_the_deadline() {
        let drain = PipeDrain::spawn(Some(std::io::Cursor::new(b"all of it".to_vec())));
        let started = Instant::now();
        let output = drain.finish(Instant::now() + Duration::from_secs(30));
        assert_eq!(output, "all of it");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(
            PipeDrain::spawn(None::<std::io::Empty>).finish(Instant::now()),
            ""
        );
    }

    /// End to end on Windows: the "scanner" exits at once but leaves a helper
    /// (`ping`, started detached) holding its stderr pipe for ~15 s. The run
    /// must come back with the stdout it wrote instead of waiting for the
    /// helper.
    #[cfg(windows)]
    #[test]
    fn a_helper_holding_the_scanner_pipes_does_not_stall_the_run() {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "echo payload& start /B ping -n 16 127.0.0.1 >NUL"]);
        let started = Instant::now();
        let capture = run_scanner_process(&mut command, Duration::from_secs(30)).expect("cmd runs");
        let elapsed = started.elapsed();
        assert!(capture.success(), "cmd should exit cleanly");
        assert!(
            capture.stdout.contains("payload"),
            "stdout={:?}",
            capture.stdout
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "run waited {elapsed:?} for the helper to release the pipe"
        );
    }
}

fn validate_au_in_child(component_id: &str) -> Result<bool, PluginScanError> {
    if let Ok(scanner) = locate_scanner_binary() {
        let mut command = Command::new(scanner);
        command
            .arg("--format")
            .arg("audiounit")
            .arg("--json")
            .arg("--validate")
            .arg(component_id);
        let timeout = bundle_scan_timeout();
        let capture = run_scanner_process(&mut command, timeout)
            .map_err(|error| PluginScanError::ScannerLaunchFailed(error.to_string()))?;
        if capture.timed_out() {
            // A component that never returns is a failed validation, not a
            // reason to stall the rest of the AudioUnit sweep.
            return Err(PluginScanError::ScannerTimedOut {
                format: PluginScanFormat::AudioUnit,
                seconds: timeout.as_secs(),
            });
        }
        if !capture.success() {
            return Err(PluginScanError::AudioUnitScannerCrashed {
                exit_code: capture.code(),
            });
        }
        return Ok(capture.stdout.contains("\"ok\":true"));
    }
    crate::au_scanner::validate_au_component(component_id)
}
