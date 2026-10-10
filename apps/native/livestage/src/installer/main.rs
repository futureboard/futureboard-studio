//! `livestage-installer`: puts LiveStage onto a computer's disk, from the
//! installer USB stick (`build.ps1 -Installer`). A text UI in the console's
//! look: choose the disk, set LiveStage up now or at its first boot, confirm
//! by typing the disk's name, install, reboot.
//!
//! ```txt
//! livestage-installer --console        # tty1 on the stick (/etc/inittab): never quits
//! livestage-installer                  # the same from a shell, with a way out
//! livestage-installer --list           # the disks, and which can take LiveStage
//! livestage-installer --disk /dev/vdb --yes [--setup-conf FILE]
//!                                      # no questions; prints progress lines
//!   --payload FILE    the image (default /usr/share/livestage/installer/livestage.img.zst,
//!                     with payload.conf next to it)
//!   --root DIR        on a copy of a machine's tree: runs no commands, and the
//!                     disk is the file DIR/dev/NAME
//! ```
//!
//! The setup's pages, the settings file and the drawing are the console's
//! own (`src/console`), shared rather than copied.

#[allow(dead_code)]
#[path = "../console/blocks.rs"]
mod blocks;
#[allow(dead_code)]
#[path = "../console/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../console/drives.rs"]
mod drives;
#[path = "../storage_api.rs"]
mod storage_api;
#[allow(dead_code)]
#[path = "../console/system.rs"]
mod system;
#[allow(dead_code)]
#[path = "../console/widgets.rs"]
mod widgets;
#[allow(dead_code)]
#[path = "../console/wizard.rs"]
mod wizard;

mod disks;
mod install;
mod payload;
mod ui;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use config::SetupConfig;
use install::{Job, Stage};
use payload::Payload;
use system::System;

const USAGE: &str = "usage: livestage-installer [--console | --list | --disk DEVICE --yes [--setup-conf FILE]] [--payload FILE] [--root DIR]";

struct Options {
    console: bool,
    list: bool,
    disk: Option<String>,
    yes: bool,
    setup_conf: Option<PathBuf>,
    payload: Option<PathBuf>,
    root: Option<PathBuf>,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        console: false,
        list: false,
        disk: None,
        yes: false,
        setup_conf: None,
        payload: None,
        root: None,
    };
    let mut args = std::env::args().skip(1);
    let value = |args: &mut dyn Iterator<Item = String>, flag: &str| {
        args.next().ok_or(format!("{flag} needs a value\n{USAGE}"))
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--console" => options.console = true,
            "--list" => options.list = true,
            "--yes" => options.yes = true,
            "--disk" => options.disk = Some(value(&mut args, "--disk")?),
            "--setup-conf" => options.setup_conf = Some(value(&mut args, "--setup-conf")?.into()),
            "--payload" => options.payload = Some(value(&mut args, "--payload")?.into()),
            "--root" => options.root = Some(value(&mut args, "--root")?.into()),
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
    let payload_path = options
        .payload
        .clone()
        .unwrap_or_else(|| system.path(payload::DEFAULT_PAYLOAD));
    let payload = Payload::load(&payload_path);

    if options.list {
        list(&system, &payload);
        return;
    }
    if let Some(disk) = &options.disk {
        let code = unattended(
            &system,
            payload,
            disk,
            options.yes,
            options.setup_conf.as_deref(),
        );
        std::process::exit(code);
    }

    if let Err(error) = run(system, payload, options.console) {
        eprintln!("livestage-installer: {error}");
        if options.console {
            // init starts it again straight away: not in a loop.
            std::thread::sleep(Duration::from_secs(5));
        }
        std::process::exit(1);
    }
}

fn run(system: System, payload: Result<Payload, String>, console: bool) -> std::io::Result<()> {
    if console && system.is_live() {
        // Kernel messages would scribble over the screen; they stay in dmesg.
        let _ = system.run("dmesg", &["-n", "1"]);
    }
    let mut terminal = ratatui::try_init()?;
    let mut app = ui::App::new(system.clone(), payload, console);
    let result = (|| {
        loop {
            terminal.draw(|frame| app.draw(frame))?;
            if app.quit {
                return Ok(());
            }
            if std::mem::take(&mut app.shell) {
                ratatui::restore();
                shell(&system);
                terminal = ratatui::try_init()?;
                terminal.clear()?;
                continue;
            }
            if app.busy() {
                app.tick();
                continue;
            }
            if event::poll(Duration::from_millis(200))? {
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

/// A shell on this terminal; the installer comes back when it ends.
fn shell(system: &System) {
    println!("\nA shell on the installer. Type exit to go back to it.\n");
    if system.is_live() {
        if let Err(error) = std::process::Command::new("/bin/sh").arg("-l").status() {
            eprintln!("/bin/sh: {error}");
            std::thread::sleep(Duration::from_secs(3));
        }
    } else {
        println!("(not run: /bin/sh -l)");
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// `--list`: every disk, and why not when it cannot take LiveStage.
fn list(system: &System, payload: &Result<Payload, String>) {
    let bytes = match payload {
        Ok(payload) => payload.bytes,
        Err(error) => {
            eprintln!("livestage-installer: {error}");
            0
        }
    };
    for disk in disks::discover(system, bytes + disks::SPARE_BYTES) {
        println!(
            "{:<10} {:>8}  {:<8} {:<24} {}{}",
            disk.device(),
            system::megabytes(disk.size_bytes),
            disk.transport.label(),
            disk.model.as_deref().unwrap_or("-"),
            disk.holds_text(),
            disk.problem
                .as_ref()
                .map(|p| format!("\n           cannot be used: {p}"))
                .unwrap_or_default()
        );
    }
}

/// `--disk DEVICE --yes`: installs with no questions, printing how it goes.
fn unattended(
    system: &System,
    payload: Result<Payload, String>,
    disk: &str,
    yes: bool,
    setup_conf: Option<&std::path::Path>,
) -> i32 {
    let payload = match payload {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("livestage-installer: no image to install: {error}");
            return 1;
        }
    };
    let name = disk.trim_start_matches("/dev/").to_string();
    let found = disks::discover(system, payload.bytes + disks::SPARE_BYTES)
        .into_iter()
        .find(|d| d.name == name);
    let Some(found) = found else {
        eprintln!("livestage-installer: there is no disk {disk} (--list shows them)");
        return 1;
    };
    if let Some(problem) = &found.problem {
        eprintln!("livestage-installer: {}: {problem}", found.device());
        return 1;
    }
    if !yes {
        eprintln!(
            "This erases everything on {} ({}). Add --yes to go ahead.",
            found.device(),
            found.title()
        );
        return 2;
    }
    let setup = match setup_conf {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => Some(SetupConfig::parse(&text)),
            Err(error) => {
                eprintln!("livestage-installer: {}: {error}", path.display());
                return 1;
            }
        },
        None => None,
    };
    println!(
        "Installing {} ({}) onto {}",
        payload.name,
        system::megabytes(payload.bytes),
        found.title()
    );
    if let Some(setup) = &setup {
        println!("  with the settings: name {}", setup.name);
    }
    let job = Job {
        disk: name,
        payload,
        setup,
        password: None,
    };
    let mut printer = Printer {
        stage: Stage::Prepare,
        since: Instant::now(),
        printed: Instant::now(),
    };
    let result = install::run(system, &job, &mut |event| printer.event(event));
    match result {
        Ok(report) => {
            for note in &report.notes {
                println!("note: {note}");
            }
            println!(
                "Done: LiveStage is on {}. Remove the USB stick, then reboot.",
                found.device()
            );
            0
        }
        Err(failure) => {
            eprintln!(
                "Failed at: {}\n  {}\n  {}",
                failure.stage.label(),
                failure.error,
                failure.state
            );
            1
        }
    }
}

/// Progress as lines: one per stage, and one every few seconds while the
/// image is written and checked.
struct Printer {
    stage: Stage,
    since: Instant,
    printed: Instant,
}

impl Printer {
    fn event(&mut self, event: install::Event) {
        match event {
            install::Event::Stage(stage) => {
                println!("==> {}", stage.label());
                self.stage = stage;
                self.since = Instant::now();
                self.printed = Instant::now();
            }
            install::Event::Progress { done, total } => {
                if self.printed.elapsed() < Duration::from_secs(3) && done < total {
                    return;
                }
                self.printed = Instant::now();
                let elapsed = self.since.elapsed().as_secs_f64().max(0.001);
                let rate = done as f64 / elapsed;
                let left = total.saturating_sub(done) as f64 / rate.max(1.0);
                println!(
                    "    {} of {} MB ({}%), {:.0} MB/s, {:.0} s left",
                    done / 1_000_000,
                    total / 1_000_000,
                    done * 100 / total.max(1),
                    rate / 1e6,
                    left
                );
            }
            install::Event::Finished(_) => {}
        }
    }
}
