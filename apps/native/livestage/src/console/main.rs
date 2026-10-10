//! `livestage-setup`: the LiveStage appliance's console.
//!
//! ```txt
//! livestage-setup --console   # tty1 (from /etc/inittab): the first setup, then the status screen
//! livestage-setup             # the same from a shell, with a way out
//! livestage-setup --setup     # straight into the setup
//! livestage-setup --boot      # at boot: the name, network and time zone onto /run
//! livestage-setup --storage-service  # root daemon: mounts drives, answers on
//!                                    # /run/livestage/storage.sock
//! livestage-setup --storage-path     # prints the recordings folder to use now
//! livestage-setup --root DIR  # any of these on a copy of the tree; runs no commands
//! ```
//!
//! The settings live in `/var/lib/livestage/setup.conf` (see [`config`]).
//! The system is read-only: what they make goes on /run, where /etc links to.
//! Where recordings go (`RECORD_STORAGE`, the drives) is [`storage`]'s.

mod config;
mod storage;
#[path = "../storage_api.rs"]
mod storage_api;
mod system;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use system::System;

const USAGE: &str = "usage: livestage-setup [--console | --setup | --boot | --storage-service | --storage-path] [--root DIR]";

struct Options {
    console: bool,
    setup: bool,
    boot: bool,
    storage_service: bool,
    storage_path: bool,
    root: Option<PathBuf>,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        console: false,
        setup: false,
        boot: false,
        storage_service: false,
        storage_path: false,
        root: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--console" => options.console = true,
            "--setup" => options.setup = true,
            "--boot" => options.boot = true,
            "--storage-service" => options.storage_service = true,
            "--storage-path" => options.storage_path = true,
            "--root" => {
                options.root = Some(PathBuf::from(
                    args.next().ok_or("--root needs a directory")?,
                ))
            }
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
    let system = System::new(options.root.clone());

    if options.storage_path {
        // Always a folder and exit 0: the internal one when the drive is not
        // there (the service must start whatever happens).
        storage::print_path(&system);
        return;
    }
    if options.storage_service {
        if let Err(error) = storage::serve(system) {
            eprintln!("livestage-setup: {error}");
            std::process::exit(1);
        }
        return;
    }

    if options.boot {
        let set_up = system.is_set_up();
        let config = system.load_config().unwrap_or_default();
        match system.write_runtime(&config) {
            Ok(()) => {
                let network = match (&config.interface, &config.fixed) {
                    (Some(port), Some(fixed)) => {
                        format!("{port} {}/{}", fixed.address, fixed.prefix)
                    }
                    (Some(port), None) => format!("{port} DHCP"),
                    (None, _) => "DHCP on every wired port".to_string(),
                };
                println!(
                    "{}, {network}, {}{}",
                    config.name,
                    config.timezone,
                    if !set_up {
                        " (not set up yet: the defaults)"
                    } else {
                        ""
                    }
                );
            }
            Err(error) => {
                eprintln!("livestage-setup: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Err(error) = run(system, options.console, options.setup) {
        eprintln!("livestage-setup: {error}");
        if options.console {
            // init starts the console again straight away: not in a loop.
            std::thread::sleep(Duration::from_secs(5));
        }
        std::process::exit(1);
    }
}

fn run(system: System, console: bool, setup: bool) -> std::io::Result<()> {
    if console && system.is_live() {
        // Kernel messages would scribble over the screen; they stay in dmesg.
        let _ = system.run("dmesg", &["-n", "1"]);
    }
    let mut terminal = ratatui::try_init()?;
    let mut app = ui::App::new(system, console, setup);
    let result = (|| {
        loop {
            terminal.draw(|frame| app.draw(frame))?;
            if app.quit {
                return Ok(());
            }
            if app.busy() {
                app.tick();
                continue;
            }
            if event::poll(Duration::from_millis(250))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key),
                    _ => {}
                }
            }
            app.tick();
            if std::mem::take(&mut app.redraw) {
                terminal.clear()?;
            }
        }
    })();
    ratatui::restore();
    result
}
