//! `livestage-setup`: the LiveStage appliance's console.
//!
//! ```txt
//! livestage-setup --console   # tty1 (from /etc/inittab): the first setup, then the status screen
//! livestage-setup             # the same from a shell, with a way out
//! livestage-setup --setup     # straight into the setup
//! livestage-setup --boot      # at boot: the name, network and time zone onto /run
//! livestage-setup --root DIR  # any of these on a copy of the tree; runs no commands
//! ```
//!
//! The settings live in `/data/system/setup.conf` (see [`config`]). The
//! system is read-only: what they make goes on /run, where /etc links to.

mod config;
mod system;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use system::System;

const USAGE: &str = "usage: livestage-setup [--console | --setup | --boot] [--root DIR]";

struct Options {
    console: bool,
    setup: bool,
    boot: bool,
    root: Option<PathBuf>,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        console: false,
        setup: false,
        boot: false,
        root: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--console" => options.console = true,
            "--setup" => options.setup = true,
            "--boot" => options.boot = true,
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

    if options.boot {
        let configured = system.load_config();
        let config = configured.clone().unwrap_or_default();
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
                    if configured.is_none() {
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
