//! Renders native Futureboard views to PNG files, without a screen capture.
//!
//! ```text
//! cargo run -p sphere_ui_components --features ui-preview --bin preview -- [OPTIONS] [SCENE…]
//!
//!   SCENE…          only these scenes (default: every scene)
//!   --out <DIR>     where the PNGs go (default: target/ui-preview)
//!   --theme <ID>    render in this theme (default: the one saved in Settings)
//!   --settle <MS>   how long each view runs before its frame is read (default: 700)
//!   --list          print the scene names and exit
//! ```
//!
//! Each scene opens a real GPUI window — Studio's view types, theme, fonts
//! and embedded assets — hidden, then moves it off the desktop and shows it
//! there without activating it, so it lays out and draws while no one sees
//! it. Its frame is then redrawn into the renderer's back buffer and copied
//! back to the CPU (`Window::render_to_image`), and written as
//! `<DIR>/<scene>.png`. The scenes themselves live in
//! [`sphere_ui_components::preview`].

use std::path::PathBuf;
use std::time::Duration;

use gpui::{AnyWindowHandle, AppContext, Application};
use sphere_ui_components::embedded_assets::EmbeddedAssets;
use sphere_ui_components::preview;

struct Options {
    out: PathBuf,
    theme: Option<String>,
    settle: Duration,
    only: Vec<String>,
    list: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        out: PathBuf::from("target/ui-preview"),
        theme: None,
        settle: Duration::from_millis(700),
        only: Vec::new(),
        list: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => {
                options.out = PathBuf::from(args.next().ok_or("--out needs a directory")?);
            }
            "--theme" => options.theme = Some(args.next().ok_or("--theme needs an id")?),
            "--settle" => {
                let ms: u64 = args
                    .next()
                    .ok_or("--settle needs milliseconds")?
                    .parse()
                    .map_err(|_| "--settle needs a whole number of milliseconds")?;
                options.settle = Duration::from_millis(ms);
            }
            "--list" => options.list = true,
            "-h" | "--help" => {
                return Err(
                    "usage: preview [--out DIR] [--theme ID] [--settle MS] [--list] [SCENE…]"
                        .into(),
                )
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            scene => options.only.push(scene.to_string()),
        }
    }
    Ok(options)
}

fn platform() -> std::rc::Rc<dyn gpui::Platform> {
    #[cfg(target_os = "windows")]
    {
        std::rc::Rc::new(
            gpui_windows::WindowsPlatform::new(false)
                .expect("failed to initialize Windows platform"),
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        compile_error!("the UI preview reads frames back from the Windows renderer only");
    }
}

fn main() {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    if options.list {
        for scene in preview::scenes() {
            println!("{}", scene.name);
        }
        return;
    }
    let scenes: Vec<_> = preview::scenes()
        .into_iter()
        .filter(|scene| options.only.is_empty() || options.only.iter().any(|n| n == scene.name))
        .collect();
    if scenes.is_empty() {
        eprintln!("no scene matches {:?}; --list shows them", options.only);
        std::process::exit(2);
    }
    if let Err(error) = std::fs::create_dir_all(&options.out) {
        eprintln!("cannot create {}: {error}", options.out.display());
        std::process::exit(1);
    }

    let Options {
        out, theme, settle, ..
    } = options;
    Application::with_platform(platform())
        .with_assets(EmbeddedAssets::new())
        .run(move |cx| {
            let active = preview::init(cx, theme.as_deref());
            println!("theme: {active}");
            cx.spawn(async move |cx| {
                let mut failed = 0;
                // The window before, closed only once the next is open: the
                // platform quits when its last window goes.
                let mut previous: Option<AnyWindowHandle> = None;
                for scene in scenes {
                    // Opened and moved off the desktop in one update, so the
                    // message loop never gets to paint it where it opened.
                    let opened = cx.update(|cx| -> Result<AnyWindowHandle, String> {
                        let handle = scene.open(cx).map_err(|error| format!("{error:#}"))?;
                        handle
                            .update(cx, |_, window, _| preview::tuck_away(window))
                            .map_err(|error| format!("{error:#}"))??;
                        Ok(handle)
                    });
                    let handle = match opened {
                        Ok(handle) => handle,
                        Err(error) => {
                            eprintln!("{}: could not open: {error}", scene.name);
                            failed += 1;
                            continue;
                        }
                    };
                    if let Some(previous) = previous.replace(handle) {
                        let _ = cx.update_window(previous, |_, window, _| window.remove_window());
                    }
                    // Let it lay out, draw, and take its first telemetry.
                    cx.background_executor().timer(settle).await;
                    let path = out.join(format!("{}.png", scene.name));
                    let saved = cx
                        .update_window(handle, |_, window, _| window.render_to_image())
                        .and_then(|frame| frame)
                        .and_then(|image| {
                            image.save(&path)?;
                            Ok((image.width(), image.height()))
                        });
                    match saved {
                        Ok((width, height)) => {
                            println!("{} → {} ({width}×{height})", scene.name, path.display())
                        }
                        Err(error) => {
                            eprintln!("{}: could not render: {error:#}", scene.name);
                            failed += 1;
                        }
                    }
                }
                cx.update(|cx| cx.quit());
                if failed > 0 {
                    std::process::exit(1);
                }
            })
            .detach();
        });
}
