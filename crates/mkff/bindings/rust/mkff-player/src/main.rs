//! MKFF Player — a desktop media player on egui / wgpu.
//!
//! ```sh
//! cargo run -p mkff-player --release -- clip.mp4
//! cargo run -p mkff-player --release -- --help
//! ```
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod renderer;
mod widgets;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use eframe::egui;
use mkff_player::time::{format_time, parse_time};
use mkff_player::{AudioOutput, PlaybackState, Player, PlayerOptions};

const USAGE: &str = "\
MKFF Player — plays any container / codec mkff-codecs decodes (no FFmpeg)

USAGE:
    mkff-player [OPTIONS] [FILE]

OPTIONS:
    --start <TIME>       Start at TIME (seconds, MM:SS or HH:MM:SS)
    --volume <0-100>     Initial volume in percent
    --mute               Start muted
    --loop               Loop playback
    --paused             Open paused on the first frame
    --speed <X>          Playback speed (0.25 - 2)
    --fullscreen         Start in full screen
    --audio <MODE>       device (default), null (simulated device), none
    --headless <SECS>    No window: play for SECS seconds and print statistics
                         (exit status 1 if >1 % of frames were dropped,
                         any were skipped, or decoding failed)
    -h, --help           Print this help
    -V, --version        Print the version

Set RUST_LOG=debug for verbose logs.";

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,mkff_player=info"),
    )
    .init();
    let (startup, headless) = match parse_args(std::env::args().skip(1)) {
        Ok(Some(v)) => v,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkff-player: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Some(seconds) = headless {
        return run_headless(&startup, seconds);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MKFF Player")
            .with_app_id("mkff-player")
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([480.0, 300.0])
            .with_drag_and_drop(true)
            .with_icon(app_icon()),
        persist_window: true,
        ..Default::default()
    };
    let result = eframe::run_native(
        "MKFF Player",
        options,
        Box::new(move |cc| Ok(Box::new(app::PlayerApp::new(cc, startup)))),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mkff-player: {e}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(
    mut args: impl Iterator<Item = String>,
) -> Result<Option<(app::Startup, Option<f64>)>, String> {
    let mut s = app::Startup {
        speed: 1.0,
        ..Default::default()
    };
    let mut headless = None;
    let value = |args: &mut dyn Iterator<Item = String>, name: &str| {
        args.next().ok_or_else(|| format!("{name} needs a value"))
    };
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("mkff-player {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--start" => {
                let v = value(&mut args, "--start")?;
                s.start = parse_time(&v).ok_or_else(|| format!("bad time {v:?}"))?;
            }
            "--volume" => {
                let v = value(&mut args, "--volume")?;
                let n: f32 = v.parse().map_err(|_| format!("bad volume {v:?}"))?;
                s.volume = Some((n / 100.0).clamp(0.0, 1.0));
            }
            "--speed" => {
                let v = value(&mut args, "--speed")?;
                s.speed = v
                    .parse::<f64>()
                    .ok()
                    .filter(|x| (0.1..=4.0).contains(x))
                    .ok_or_else(|| format!("bad speed {v:?}"))?;
            }
            "--audio" => {
                s.audio = match value(&mut args, "--audio")?.as_str() {
                    "device" => AudioOutput::Device,
                    "null" => AudioOutput::Null,
                    "none" | "off" => AudioOutput::Disabled,
                    other => return Err(format!("bad audio mode {other:?}")),
                }
            }
            "--headless" => {
                let v = value(&mut args, "--headless")?;
                headless = Some(
                    v.parse::<f64>()
                        .ok()
                        .filter(|x| x.is_finite() && *x > 0.0)
                        .ok_or_else(|| format!("bad duration {v:?}"))?,
                );
            }
            "--no-audio" => s.audio = AudioOutput::Disabled,
            "--mute" => s.muted = true,
            "--loop" => s.looping = true,
            "--paused" => s.paused = true,
            "--fullscreen" => s.fullscreen = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            file => {
                if s.file.is_some() {
                    return Err("only one file can be played".into());
                }
                s.file = Some(PathBuf::from(file));
            }
        }
    }
    Ok(Some((s, headless)))
}

/// Play without a window, the way the GUI would, and report how it went.
fn run_headless(s: &app::Startup, seconds: f64) -> ExitCode {
    let Some(path) = &s.file else {
        eprintln!("mkff-player: --headless needs a FILE");
        return ExitCode::from(2);
    };
    let t0 = Instant::now();
    let media = match mkff_player::open_media(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mkff-player: {e}");
            return ExitCode::FAILURE;
        }
    };
    let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut player = Player::new(
        media,
        PlayerOptions {
            audio: s.audio,
            volume: s.volume.unwrap_or(1.0),
            muted: s.muted,
            looping: s.looping,
            speed: s.speed,
            start: s.start,
            autoplay: true,
            ..PlayerOptions::default()
        },
    );
    let info = player.info().clone();
    println!(
        "{}: {}, {:.3} s, opened in {open_ms:.0} ms",
        path.display(),
        info.container,
        player.duration()
    );
    if let Some(v) = &info.video {
        println!(
            "  video: {} {}x{} @ {:.3} fps, {}",
            v.codec,
            v.width,
            v.height,
            player.frame_rate(),
            v.pixel_format
        );
    }
    for a in &info.audio {
        println!(
            "  audio: {} {} Hz, {} ch",
            a.codec, a.sample_rate, a.channels
        );
    }

    let start = Instant::now();
    let mut max_offset: f64 = 0.0;
    let mut last_report = Instant::now();
    let mut last_update = Instant::now();
    let mut max_gap = Duration::ZERO;
    while start.elapsed().as_secs_f64() < seconds {
        max_gap = max_gap.max(last_update.elapsed());
        last_update = Instant::now();
        player.update();
        for e in player.take_events() {
            println!("  event: {e}");
        }
        if player.is_playing() && player.current_picture().is_some() {
            max_offset = max_offset.max(player.stats().av_offset.abs());
        }
        if player.state() == PlaybackState::Ended {
            println!(
                "  reached the end at {}",
                format_time(player.position(), true)
            );
            break;
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            log::info!("position {}", format_time(player.position(), true));
        }
        // a 120 Hz display
        std::thread::sleep(Duration::from_micros(8333));
    }
    let wall = start.elapsed().as_secs_f64();
    let st = player.stats();
    println!(
        "  played {} in {:.3} s wall ({}, {})",
        format_time(player.position(), true),
        wall,
        if st.audio_master {
            "audio clock"
        } else {
            "system clock"
        },
        st.audio_device.as_deref().unwrap_or("no audio output")
    );
    println!(
        "  frames: decoded {} shown {} dropped {} skipped {} errors {}; decode {:.2} ms/frame",
        st.decoded, st.presented, st.dropped, st.skipped, st.video_errors, st.decode_ms
    );
    println!(
        "  audio: underruns {} errors {}; max |A-V| {:.1} ms; longest update gap {:.1} ms",
        st.audio_underruns,
        st.audio_errors,
        max_offset * 1000.0,
        max_gap.as_secs_f64() * 1000.0
    );
    // a rare late frame is the scheduler's doing; more than 1 % is not
    if st.dropped * 100 > st.presented.max(1) || st.skipped > 0 || st.video_errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// A rounded square with a play triangle.
fn app_icon() -> egui::IconData {
    const N: usize = 64;
    let mut rgba = vec![0u8; N * N * 4];
    let r = 14.0f32;
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            // rounded-rect coverage
            let dx = (fx - N as f32 / 2.0).abs() - (N as f32 / 2.0 - r);
            let dy = (fy - N as f32 / 2.0).abs() - (N as f32 / 2.0 - r);
            let d = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() - r;
            let a = (0.5 - d).clamp(0.0, 1.0);
            // play triangle (22,16)-(48,32)-(22,48)
            let inside = fx >= 22.0 && (fy - 32.0).abs() <= (48.0 - fx) * (16.0 / 26.0);
            let t = fy / N as f32;
            let (cr, cg, cb) = if inside {
                (255.0, 255.0, 255.0)
            } else {
                (
                    40.0 + 39.0 * (1.0 - t),
                    70.0 + 71.0 * (1.0 - t),
                    160.0 + 95.0 * (1.0 - t),
                )
            };
            let i = (y * N + x) * 4;
            rgba[i..i + 4].copy_from_slice(&[cr as u8, cg as u8, cb as u8, (a * 255.0) as u8]);
        }
    }
    egui::IconData {
        rgba,
        width: N as u32,
        height: N as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Option<(app::Startup, Option<f64>)>, String> {
        parse_args(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn cli() {
        let (s, h) = args(&["--start", "1:30", "--volume", "50", "--loop", "a.mp4"])
            .unwrap()
            .unwrap();
        assert_eq!(s.start, 90.0);
        assert_eq!(s.volume, Some(0.5));
        assert!(s.looping);
        assert_eq!(s.file, Some(PathBuf::from("a.mp4")));
        assert_eq!(h, None);
        let (s, h) = args(&["--audio", "null", "--headless", "2", "b.mkv"])
            .unwrap()
            .unwrap();
        assert_eq!(s.audio, AudioOutput::Null);
        assert_eq!(h, Some(2.0));
        assert!(args(&["--bogus"]).is_err());
        assert!(args(&["a", "b"]).is_err());
        assert!(args(&["--speed", "9"]).is_err());
        assert!(args(&["--help"]).unwrap().is_none());
    }
}
