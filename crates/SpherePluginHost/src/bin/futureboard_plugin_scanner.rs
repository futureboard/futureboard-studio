#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process;

use SpherePluginHost::au_scanner;
use SpherePluginHost::scan::isolation::{run_direct_format_scan_for_cli, SCAN_PAYLOAD_SENTINEL};
use SpherePluginHost::scan::types::PluginScanFormat;

/// Keeps the scan payload separate from anything a plug-in module prints.
///
/// Loading a module runs vendor code that writes to this process's stdout —
/// Kontakt 8 logs two `[info]` lines through the C runtime — and that text
/// landed in front of the JSON, so the parent's parse failed and every class in
/// the bundle was discarded. `stdout_guard` points file descriptor 1 at stderr
/// for the duration of the scan and keeps a private duplicate of the real
/// stdout, so plug-in chatter is still captured (as stderr, where the parent
/// logs it) but can never reach the payload channel.
mod stdout_guard {
    use std::io::Write;
    use std::mem::ManuallyDrop;

    /// A writer aimed at the process's *original* stdout, kept alive after file
    /// descriptor 1 has been pointed at stderr.
    ///
    /// The descriptor is never closed: the process exits immediately after the
    /// payload is written, and on Windows the duplicated CRT descriptor and the
    /// OS handle refer to the same object, so closing one here would leave the
    /// runtime's own teardown closing a stale handle.
    pub struct PayloadStdout(Option<ManuallyDrop<std::fs::File>>);

    impl Write for PayloadStdout {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self.0.as_mut() {
                Some(file) => file.write(buf),
                None => std::io::stdout().write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self.0.as_mut() {
                Some(file) => file.flush(),
                None => std::io::stdout().flush(),
            }
        }
    }

    #[cfg(unix)]
    pub fn redirect_plugin_output_to_stderr() -> PayloadStdout {
        use std::os::fd::FromRawFd;
        unsafe {
            let saved = libc::dup(libc::STDOUT_FILENO);
            if saved < 0 {
                return PayloadStdout(None);
            }
            // Never handed down to a helper process a plug-in starts: one
            // that outlives the scan would keep the parent's pipe open.
            libc::fcntl(saved, libc::F_SETFD, libc::FD_CLOEXEC);
            if libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
                libc::close(saved);
                return PayloadStdout(None);
            }
            PayloadStdout(Some(ManuallyDrop::new(std::fs::File::from_raw_fd(saved))))
        }
    }

    #[cfg(windows)]
    pub fn redirect_plugin_output_to_stderr() -> PayloadStdout {
        use std::os::windows::io::FromRawHandle;

        unsafe extern "C" {
            #[link_name = "_dup"]
            fn c_dup(fd: i32) -> i32;
            #[link_name = "_dup2"]
            fn c_dup2(src: i32, dst: i32) -> i32;
            #[link_name = "_close"]
            fn c_close(fd: i32) -> i32;
            #[link_name = "_get_osfhandle"]
            fn c_get_osfhandle(fd: i32) -> isize;
        }

        const STDOUT_FD: i32 = 1;
        const STDERR_FD: i32 = 2;

        unsafe {
            let saved = c_dup(STDOUT_FD);
            if saved < 0 {
                return PayloadStdout(None);
            }
            if c_dup2(STDERR_FD, STDOUT_FD) < 0 {
                c_close(saved);
                return PayloadStdout(None);
            }
            let handle = c_get_osfhandle(saved);
            if handle == -1 || handle == -2 {
                c_close(saved);
                return PayloadStdout(None);
            }
            // `_dup` makes an inheritable duplicate. A plug-in that starts a
            // resident helper (a licensing daemon) with handle inheritance on
            // would then keep the parent's payload pipe open for as long as
            // the helper lives; the payload channel is never the helper's.
            let _ = windows::Win32::Foundation::SetHandleInformation(
                windows::Win32::Foundation::HANDLE(handle as *mut _),
                windows::Win32::Foundation::HANDLE_FLAG_INHERIT.0,
                windows::Win32::Foundation::HANDLE_FLAGS(0),
            );
            PayloadStdout(Some(ManuallyDrop::new(std::fs::File::from_raw_handle(
                handle as *mut _,
            ))))
        }
    }

    #[cfg(not(any(unix, windows)))]
    pub fn redirect_plugin_output_to_stderr() -> PayloadStdout {
        PayloadStdout(None)
    }
}

/// Write one framed payload line and flush it. The sentinel lets the parent find
/// the payload even if something wrote to the real stdout handle directly
/// (bypassing the descriptor redirect above).
fn emit_payload(mut stdout: stdout_guard::PayloadStdout, json: &str) {
    let _ = write!(stdout, "\n{SCAN_PAYLOAD_SENTINEL}{json}\n");
    let _ = stdout.flush();
}

fn main() {
    let mut format: Option<PluginScanFormat> = None;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut validate_plugins = false;
    let mut validate_component: Option<String> = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                let value = args.next().unwrap_or_default();
                format = PluginScanFormat::from_cli(&value);
                if format.is_none() {
                    eprintln!("Unknown format: {value}");
                    process::exit(2);
                }
            }
            "--json" => {}
            "--path" => {
                if let Some(path) = args.next() {
                    paths.push(PathBuf::from(path));
                }
            }
            "--validate-plugins" => validate_plugins = true,
            "--validate" => {
                validate_component = args.next();
            }
            "--help" | "-h" => {
                print_help();
                process::exit(0);
            }
            other => {
                eprintln!("Unknown argument: {other}");
                print_help();
                process::exit(2);
            }
        }
    }

    let Some(format) = format else {
        eprintln!("Missing required --format");
        print_help();
        process::exit(2);
    };

    // Everything below this point may load plug-in binaries, so stdout is
    // claimed for the payload before the first module is opened.
    let stdout = stdout_guard::redirect_plugin_output_to_stderr();

    if let Some(component_id) = validate_component {
        if format != PluginScanFormat::AudioUnit {
            eprintln!("--validate is only supported for audiounit");
            process::exit(2);
        }
        match au_scanner::validate_au_component(&component_id) {
            Ok(ok) => {
                emit_payload(
                    stdout,
                    if ok {
                        "{\"ok\":true}"
                    } else {
                        "{\"ok\":false}"
                    },
                );
                process::exit(if ok { 0 } else { 1 });
            }
            Err(error) => {
                emit_payload(
                    stdout,
                    &format!(
                        "{{\"ok\":false,\"error\":{}}}",
                        serde_json::to_string(&error.message())
                            .unwrap_or_else(|_| "\"error\"".into())
                    ),
                );
                process::exit(1);
            }
        }
    }

    let payload = run_direct_format_scan_for_cli(format, &paths, validate_plugins);
    match serde_json::to_string(&payload) {
        Ok(json) => {
            emit_payload(stdout, &json);
            process::exit(if payload.process_crashed { 1 } else { 0 });
        }
        Err(error) => {
            eprintln!("Failed to serialize scan result: {error}");
            process::exit(1);
        }
    }
}

fn print_help() {
    eprintln!(
        "FutureboardPluginScanner\n\
         Usage:\n\
           FutureboardPluginScanner --format vst3|clap|audiounit --json [--path <dir>]...\n\
           FutureboardPluginScanner --format audiounit --json --validate <component-id>\n\
           FutureboardPluginScanner --format audiounit --json --validate-plugins"
    );
}
