//! Manual check: run the Studio API server without Studio, feeding the master
//! visualizer tap a synthetic stereo signal, and open the page it serves.
//!
//! ```text
//! bun run build:visualizer-web
//! cargo run -p sphere_ui_components --example api_server_demo -- [port] [seconds]
//! ```
//!
//! Then open http://127.0.0.1:7471 (or the port given). The signal is a
//! chord with a slow swell and a stereo drift, so every view moves: loudness
//! readouts and history, spectrum, levels, correlation and the vectorscope.

use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(sphere_ui_components::api_server::DEFAULT_PORT);
    let seconds: u64 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);

    sphere_ui_components::api_server::apply_settings(true, port);
    match sphere_ui_components::api_server::status() {
        sphere_ui_components::api_server::ApiServerStatus::Running(addr) => {
            println!("API server on http://{addr} for {seconds} s");
        }
        other => {
            eprintln!("API server did not start: {other:?}");
            std::process::exit(1);
        }
    }

    const RATE: u32 = 48_000;
    const BLOCK: usize = 480;
    let tap = DirectAudio::visualizer_tap();
    let started = Instant::now();
    let mut frame: u64 = 0;
    let mut block = vec![0.0f32; BLOCK * 2];
    let notes = [110.0f32, 220.0, 277.18, 329.63, 440.0, 1_760.0];
    while started.elapsed() < Duration::from_secs(seconds) {
        let block_started = Instant::now();
        for i in 0..BLOCK {
            let t = (frame + i as u64) as f32 / RATE as f32;
            let swell = 0.25 + 0.2 * (t * 0.4 * std::f32::consts::TAU).sin();
            let pan = 0.5 + 0.35 * (t * 0.13 * std::f32::consts::TAU).sin();
            let mut voice = 0.0;
            for (n, hz) in notes.iter().enumerate() {
                voice += (t * hz * std::f32::consts::TAU).sin() / (n as f32 + 1.5);
            }
            let noise =
                ((frame + i as u64).wrapping_mul(2_654_435_761) % 1_000) as f32 / 1_000.0 - 0.5;
            let sample = (voice * 0.5 + noise * 0.04) * swell;
            block[i * 2] = sample * (1.0 - pan) * 1.4;
            block[i * 2 + 1] = sample * pan * 1.4;
        }
        tap.write_interleaved(&block, 2, RATE);
        frame += BLOCK as u64;
        let period = Duration::from_secs_f64(BLOCK as f64 / RATE as f64);
        std::thread::sleep(period.saturating_sub(block_started.elapsed()));
    }
    sphere_ui_components::api_server::shutdown();
}
