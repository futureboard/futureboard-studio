//! LiveStage without a window.
//!
//! Runs a session on the interface and takes commands as JSON lines on stdin,
//! answering each with one JSON line on stdout. Meant for a rack machine, a
//! Raspberry Pi under the stage, or a Linux server with no desktop — there it
//! is built with `--no-default-features` and runs the built-in effects only.
//!
//! ```txt
//! livestage-server --list-devices
//! livestage-server --session show.json --output "hw:CARD=USB" --rate 48000
//! livestage-server --session show.json --daemon        # no stdin; stop with SIGTERM
//! ```
//!
//! Commands are the engine's ([`livestage_engine::Command`]) plus a few for
//! the server itself:
//!
//! ```txt
//! {"cmd":"set_fader","strip":{"kind":"channel","id":1},"db":-6}
//! {"cmd":"start_recording"}
//! {"cmd":"status"}   {"cmd":"meters"}   {"cmd":"session"}   {"cmd":"effects"}
//! {"cmd":"save"}     {"cmd":"save","path":"other.json"}     {"cmd":"quit"}
//! ```

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use livestage_engine::{Command, LiveEngine, Session};
use serde_json::{Value, json};

static STOP: AtomicBool = AtomicBool::new(false);

struct Options {
    session: Option<PathBuf>,
    host: Option<String>,
    input: Option<String>,
    output: Option<String>,
    rate: Option<u32>,
    buffer: Option<u32>,
    record_dir: Option<PathBuf>,
    list_devices: bool,
    daemon: bool,
    autosave: bool,
}

const USAGE: &str = "usage: livestage-server [--session FILE] [--host NAME] [--input DEVICE] \
[--output DEVICE] [--rate HZ] [--buffer FRAMES] [--record-dir DIR] [--daemon] [--no-autosave] \
[--list-devices]";

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        session: None,
        host: None,
        input: None,
        output: None,
        rate: None,
        buffer: None,
        record_dir: None,
        list_devices: false,
        daemon: false,
        autosave: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--session" => options.session = Some(PathBuf::from(value("--session")?)),
            "--host" => options.host = Some(value("--host")?),
            "--input" => options.input = Some(value("--input")?),
            "--output" => options.output = Some(value("--output")?),
            "--rate" => {
                options.rate = Some(
                    value("--rate")?
                        .parse()
                        .map_err(|_| "--rate needs a number")?,
                )
            }
            "--buffer" => {
                options.buffer = Some(
                    value("--buffer")?
                        .parse()
                        .map_err(|_| "--buffer needs a number")?,
                )
            }
            "--record-dir" => options.record_dir = Some(PathBuf::from(value("--record-dir")?)),
            "--list-devices" => options.list_devices = true,
            "--daemon" => options.daemon = true,
            "--no-autosave" => options.autosave = false,
            "-h" | "--help" => return Err(USAGE.to_string()),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(options)
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    if options.list_devices {
        list_devices(options.host.as_deref());
        return;
    }

    let mut session = match &options.session {
        Some(path) if path.exists() => match Session::load(path) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("[livestage] {error}");
                std::process::exit(1);
            }
        },
        _ => Session::default(),
    };
    let fresh = options.session.as_ref().is_none_or(|path| !path.exists());
    if let Some(host) = &options.host {
        session.audio.host = Some(host.clone());
    }
    if let Some(input) = &options.input {
        session.audio.input_device = Some(input.clone());
    }
    if let Some(output) = &options.output {
        session.audio.output_device = Some(output.clone());
    }
    if let Some(rate) = options.rate {
        session.audio.sample_rate = rate;
    }
    if let Some(buffer) = options.buffer {
        session.audio.buffer_frames = buffer;
    }
    if let Some(dir) = &options.record_dir {
        session.recording.folder = Some(dir.clone());
    }

    let mut engine = LiveEngine::new(session);
    if fresh {
        // No file yet: one channel per interface input, faders down.
        let inputs = engine.input_channels();
        for index in 0..inputs {
            let _ = engine.apply(Command::AddChannel {
                name: format!("Input {}", index + 1),
                input: livestage_engine::InputPatch::mono(index as u16),
            });
        }
    }
    let status = engine.status();
    eprintln!(
        "[livestage] {} - {} Hz, {} in / {} out{}",
        status.output_device.as_deref().unwrap_or("no device"),
        status.sample_rate,
        status.in_channels,
        status.out_channels,
        status
            .error
            .as_deref()
            .map(|e| format!(" - {e}"))
            .unwrap_or_default()
    );

    install_stop_handler();
    let commands = if options.daemon {
        None
    } else {
        Some(spawn_stdin_reader())
    };
    let stdout = std::io::stdout();
    loop {
        if STOP.load(Ordering::Relaxed) {
            break;
        }
        engine.poll();
        let Some(commands) = &commands else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        match commands.recv_timeout(Duration::from_millis(50)) {
            Ok(line) => {
                let (reply, quit) = handle(&mut engine, &options, &line);
                let mut out = stdout.lock();
                let _ = writeln!(out, "{reply}");
                let _ = out.flush();
                if quit {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // stdin closed: whoever drove us is gone.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if let Some(summary) = engine.stop_recording() {
        eprintln!(
            "[livestage] recording stopped: {} file(s) in {}",
            summary.files.len(),
            summary.folder.display()
        );
    }
    if options.autosave {
        if let Some(path) = &options.session {
            match engine.save(path, Duration::from_secs(2)) {
                Ok(()) => eprintln!("[livestage] session saved to {}", path.display()),
                Err(error) => eprintln!("[livestage] session not saved: {error}"),
            }
        }
    }
}

fn spawn_stdin_reader() -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Answer one command line. Returns the reply and whether to quit.
fn handle(engine: &mut LiveEngine, options: &Options, line: &str) -> (Value, bool) {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => return (json!({"ok": false, "error": error.to_string()}), false),
    };
    let reply = match value.get("cmd").and_then(Value::as_str) {
        Some("quit") => return (json!({"ok": true}), true),
        Some("status") => json!({"ok": true, "status": engine.status()}),
        Some("session") => json!({"ok": true, "session": engine.session()}),
        Some("meters") => {
            let meters: Vec<Value> = engine
                .meters()
                .into_iter()
                .map(|(strip, levels)| json!({"strip": strip, "levels": levels}))
                .collect();
            json!({"ok": true, "meters": meters})
        }
        Some("effects") => {
            let effects: Vec<Value> = livestage_engine::builtin_fx::BUILTIN_EFFECTS
                .iter()
                .map(|e| json!({"stem": e.stem, "name": e.name, "category": e.category}))
                .collect();
            json!({"ok": true, "effects": effects})
        }
        #[cfg(feature = "external-plugins")]
        Some("installed") => {
            // The third-party effects Studio's scanner found on this machine.
            let effects: Vec<Value> = livestage_engine::external::installed_effects()
                .into_iter()
                .map(|e| {
                    json!({
                        "name": e.name,
                        "vendor": e.vendor,
                        "format": e.format,
                        "path": e.path,
                        "class_id": e.class_id,
                    })
                })
                .collect();
            json!({"ok": true, "installed": effects})
        }
        Some("inserts") => {
            // Every insert and whether it runs: a third-party one loads in
            // the plug-in host and can fail there.
            let engine: &LiveEngine = engine;
            let session = engine.session();
            let cores = session
                .channels
                .iter()
                .map(|c| (json!({"kind": "channel", "id": c.id}), &c.core))
                .chain(
                    session
                        .buses
                        .iter()
                        .map(|b| (json!({"kind": "bus", "id": b.id}), &b.core)),
                )
                .chain(std::iter::once((
                    json!({"kind": "master"}),
                    &session.master.core,
                )));
            let inserts: Vec<Value> = cores
                .flat_map(|(strip, core)| {
                    core.inserts.iter().map(move |slot| {
                        let state = match engine.insert_state(slot.id) {
                            livestage_engine::InsertState::Ready => json!("ready"),
                            livestage_engine::InsertState::Loading => json!("loading"),
                            livestage_engine::InsertState::Failed(error) => {
                                json!({"failed": error})
                            }
                        };
                        json!({
                            "strip": strip,
                            "id": slot.id,
                            "name": slot.plugin.display_name(),
                            "bypass": slot.bypass,
                            "state": state,
                        })
                    })
                })
                .collect();
            json!({"ok": true, "inserts": inserts})
        }
        Some("save") => {
            let path = value
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| options.session.clone());
            match path {
                Some(path) => match engine.save(&path, Duration::from_secs(2)) {
                    Ok(()) => json!({"ok": true, "path": path}),
                    Err(error) => json!({"ok": false, "error": error}),
                },
                None => json!({"ok": false, "error": "no session path: pass \"path\""}),
            }
        }
        Some("stop_recording") => match engine.stop_recording() {
            Some(summary) => json!({
                "ok": true,
                "folder": summary.folder,
                "files": summary.files,
                "seconds": summary.seconds,
                "dropped_samples": summary.dropped_samples,
                "errors": summary.errors,
            }),
            None => json!({"ok": false, "error": "not recording"}),
        },
        _ => match serde_json::from_value::<Command>(value) {
            Ok(command) => match engine.apply(command) {
                Ok(()) => json!({"ok": true}),
                Err(error) => json!({"ok": false, "error": error}),
            },
            Err(error) => json!({"ok": false, "error": error.to_string()}),
        },
    };
    (reply, false)
}

fn list_devices(host: Option<&str>) {
    let hosts = livestage_engine::device::host_names();
    let (inputs, outputs) = livestage_engine::device::list_devices(host);
    let describe = |devices: &[livestage_engine::device::DeviceInfo]| -> Vec<Value> {
        devices
            .iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "channels": d.channels,
                    "default_sample_rate": d.default_sample_rate,
                    "default": d.is_default,
                })
            })
            .collect()
    };
    let report = json!({
        "hosts": hosts,
        "inputs": describe(&inputs),
        "outputs": describe(&outputs),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
}

/// Ctrl+C, SIGTERM or a console close stop the server cleanly: the take
/// being recorded is closed and the session saved.
fn install_stop_handler() {
    #[cfg(unix)]
    unsafe {
        extern "C" fn on_signal(_: libc::c_int) {
            STOP.store(true, Ordering::Relaxed);
        }
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    #[cfg(windows)]
    unsafe {
        unsafe extern "system" fn on_ctrl(_: u32) -> windows_sys::core::BOOL {
            STOP.store(true, Ordering::Relaxed);
            // Handled: give the main loop time to finish the take.
            std::thread::sleep(Duration::from_secs(3));
            1
        }
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}
