//! How many bridged built-in blocks come back late at small device buffers
//! (#420), against real spawned `FutureboardPluginHostX64` processes:
//!
//! ```text
//! cargo build --release -p sphere-plugin-host --features plugin-host-bin --bin FutureboardPluginHostX64
//! $env:FUTUREBOARD_PLUGIN_HOST_EXE = "<target>\release\FutureboardPluginHostX64.exe"
//! cargo test -p sphere-plugin-host --features plugin-host-bin --test bridge_shared_host_timing \
//!   --config "profile.dev.package.'*'.opt-level=3" -- --ignored --nocapture
//! ```
//!
//! A release host, because a debug one overflows its main thread's stack
//! loading the larger built-ins, and because its timings are the point.
//! `TIMING_SECONDS` (default 8) sets each run's length, `TIMING_LOAD` (0.5)
//! the share of the period the simulated graph render takes, and
//! `TIMING_FIXED_ORDER` asks for every insert at the same moment of every
//! callback instead.
//!
//! What is real: the host processes, their audio producer threads and kick
//! events, the shared-memory regions, the engine's realtime sink
//! ([`SharedRegionSink`]) and the built-ins' DSP. What is simulated: the
//! device. A thread at MMCSS "Pro Audio" plays the device callback at the
//! period of a 128- or 256-frame buffer, and does for each insert exactly what
//! the engine's callback does — read the block the host finished (with the
//! sink's own handoff grace), hand it the next dry block, ask for it. A read
//! that comes back empty is a block the engine would have bypassed: an audible
//! jump in that track.
//!
//! The same chain is played through one shared host (the default) and through
//! one host per plug-in (`FUTUREBOARD_PLUGIN_HOST_ISOLATION=instance`), which
//! #420 found to play clean.
#![cfg(all(feature = "plugin-host-bin", windows))]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use DirectAudio::plugin_bridge::SharedPluginBridgeSink;
use SpherePluginHost::audio_bridge::{bridge_kick_event_name, BridgeKickEvent, SharedAudioRegion};
use SpherePluginHost::ipc::HostEvent;
use SpherePluginHost::plugin_bridge_sink::SharedRegionSink;
use SpherePluginHost::plugin_host_client::{ClientEvent, PluginHostClient};

const RATE: u32 = 48_000;

/// A session's worth of built-ins, as several tracks would carry them.
const CHAIN: &[&str] = &[
    "rodharerist",
    "equz8",
    "burnlimit",
    "zcomp",
    "equzx",
    "verbspace",
    "echospace",
    "compresser",
    "deesser",
    "airboost",
    "clipper67",
    "transient",
];

fn wait_for<T>(
    client: &PluginHostClient,
    what: &str,
    mut pick: impl FnMut(&HostEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match client.try_recv_event() {
            Some(ClientEvent::Host(event)) => {
                if let Some(found) = pick(&event) {
                    return found;
                }
            }
            Some(ClientEvent::Disconnected) => panic!("host went away waiting for {what}"),
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    panic!("host did not answer: {what}");
}

struct Host {
    client: PluginHostClient,
    kick: Arc<BridgeKickEvent>,
}

impl Host {
    fn spawn() -> Self {
        let client = PluginHostClient::spawn().expect("spawn FutureboardPluginHostX64");
        let name = bridge_kick_event_name(std::process::id(), client.pid());
        let kick = Arc::new(BridgeKickEvent::create_named(&name).expect("kick event"));
        Self { client, kick }
    }
}

struct Insert {
    region: Arc<SharedAudioRegion>,
    sink: SharedPluginBridgeSink,
}

fn load(host: &mut Host, stem: &str, block: u32) -> Insert {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = format!("timing-{stem}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let name = format!("Local\\FutureboardAudioBridge-{}__{id}", std::process::id());
    let region =
        Arc::new(SharedAudioRegion::create_named(&name, RATE, block, 2).expect("create region"));
    host.client
        .attach_shared_audio(name, region.bytes(), &id)
        .expect("send AttachSharedAudio");
    let attached = wait_for(&host.client, "SharedAudioAttached", |event| match event {
        HostEvent::SharedAudioAttached { attached, .. } => Some(*attached),
        _ => None,
    });
    assert!(attached, "host mapped the region");
    host.client
        .load_builtin_plugin(&id, stem, RATE, block, None)
        .expect("send LoadBuiltinPlugin");
    wait_for(&host.client, "PluginLoaded", |event| match event {
        HostEvent::PluginLoaded { .. } => Some(()),
        HostEvent::PluginLoadFailed { error, .. } => panic!("{stem}: load failed: {error}"),
        _ => None,
    });
    Insert {
        sink: SharedRegionSink::into_shared(region.clone(), Some(host.kick.clone())),
        region,
    }
}

#[link(name = "avrt")]
extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
}
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
}

/// The device thread's priority: what a driver thread gets.
fn promote_like_a_device_thread() {
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut index = 0u32;
    // SAFETY: NUL-terminated UTF-16 task name, valid out pointer.
    unsafe {
        timeBeginPeriod(1);
        AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut index);
    }
}

#[derive(Default, Debug)]
struct Run {
    callbacks: u64,
    /// Insert blocks the engine would have bypassed.
    late: u64,
    /// Callbacks with at least one late insert.
    late_callbacks: u64,
    /// Callbacks whose own work overran the period.
    overruns: u64,
    worst_callback_us: u128,
    /// Producer time all the inserts' last blocks took together: what one
    /// shared host has to fit into a period. Mean and worst, µs.
    host_sum_us: u64,
    worst_host_period_us: u32,
}

impl Run {
    fn mean_host_period_us(&self) -> u64 {
        self.host_sum_us / self.callbacks.max(1)
    }
}

/// Busy the device thread for `micros`, as the graph's own render would.
fn render_for(micros: u64) {
    let until = Instant::now() + Duration::from_micros(micros);
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

/// Play `seconds` of device callbacks over `inserts`.
///
/// `engine_like` plays each callback as the engine's render does: the graph's
/// own work takes up to `render_load` of the period, and each insert is reached at a
/// different point of it from one callback to the next (tracks are claimed by
/// the pool's threads in whatever order they get to them, and clips, sends
/// and buses cost more on some blocks than others). Off, every insert is
/// asked for at the same moment of every callback.
fn play(
    inserts: &[Insert],
    block: usize,
    seconds: f32,
    engine_like: bool,
    render_load: f32,
) -> Run {
    promote_like_a_device_thread();
    let period = Duration::from_nanos(block as u64 * 1_000_000_000 / u64::from(RATE));
    let mut l = vec![0.0f32; block];
    let mut r = vec![0.0f32; block];
    let mut seed = 0x1234_5678u32;
    let mut dry = vec![0.0f32; block];
    let mut run = Run::default();
    let warmup = 200u64;
    let total = warmup + (seconds * RATE as f32 / block as f32) as u64;
    let mut order: Vec<usize> = (0..inserts.len()).collect();
    let mut next = Instant::now() + period;
    for callback in 0..total {
        // Wait for the device: sleep most of the way, spin the rest.
        loop {
            let now = Instant::now();
            if now >= next {
                break;
            }
            if next - now > Duration::from_micros(1500) {
                std::thread::sleep(Duration::from_millis(1));
            } else {
                std::hint::spin_loop();
            }
        }
        let started = Instant::now();
        next += period;
        for sample in dry.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *sample = (seed as f32 / u32::MAX as f32 - 0.5) * 0.2;
        }
        let mut late_here = false;
        // This callback's order and the render work before each insert.
        for slot in (1..order.len()).rev().filter(|_| engine_like) {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            order.swap(slot, seed as usize % (slot + 1));
        }
        let budget = (period.as_micros() as f32 * render_load) as u64;
        for &index in &order {
            let insert = &inserts[index];
            if engine_like {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                render_for(u64::from(seed) % (2 * budget / inserts.len() as u64 + 1));
            }
            let got = insert.sink.read_output(&mut l, &mut r, block);
            let can_publish = got > 0 || !insert.sink.request_in_flight();
            if can_publish {
                insert.sink.write_input(&dry, &dry, block);
                insert.sink.request_block(block as u32);
            }
            if callback >= warmup && got == 0 {
                run.late += 1;
                late_here = true;
            }
        }
        if callback < warmup {
            continue;
        }
        run.callbacks += 1;
        run.late_callbacks += u64::from(late_here);
        let host_period: u32 = inserts
            .iter()
            .map(|insert| {
                insert
                    .region
                    .bridge()
                    .last_process_micros
                    .load(Ordering::Relaxed)
            })
            .sum();
        run.host_sum_us += u64::from(host_period);
        run.worst_host_period_us = run.worst_host_period_us.max(host_period);
        let took = started.elapsed();
        run.worst_callback_us = run.worst_callback_us.max(took.as_micros());
        if took > period {
            run.overruns += 1;
        }
        if Instant::now() > next {
            // The device would have glitched anyway; start the clock again.
            next = Instant::now() + period;
        }
    }
    run
}

fn measure(
    per_instance: bool,
    block: u32,
    seconds: f32,
    engine_like: bool,
    render_load: f32,
) -> Run {
    let mut hosts: Vec<Host> = Vec::new();
    let mut inserts = Vec::new();
    for stem in CHAIN {
        if per_instance || hosts.is_empty() {
            hosts.push(Host::spawn());
        }
        let host = hosts.last_mut().expect("host");
        inserts.push(load(host, stem, block));
    }
    let block = block as usize;
    let run = std::thread::spawn({
        let inserts = std::mem::take(&mut inserts);
        move || {
            let run = play(&inserts, block, seconds, engine_like, render_load);
            drop(inserts);
            run
        }
    })
    .join()
    .expect("device thread");
    for host in &mut hosts {
        host.client.shutdown().ok();
    }
    run
}

#[test]
#[ignore = "measurement against real host processes; run with --ignored --nocapture"]
fn bridged_builtins_at_small_buffers() {
    let seconds = std::env::var("TIMING_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8.0);
    let engine_like = std::env::var_os("TIMING_FIXED_ORDER").is_none();
    let render_load: f32 = std::env::var("TIMING_LOAD")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0.5);
    println!();
    println!(
        "{} built-ins at {RATE} Hz, {seconds} s each, {}",
        CHAIN.len(),
        if engine_like {
            format!("engine-like order, render load {render_load}")
        } else {
            "fixed order, no render load".to_string()
        }
    );
    println!(
        "{:>6} {:>12} {:>9} {:>8} {:>10} {:>9} {:>13} {:>13} {:>13}",
        "block",
        "hosts",
        "callbacks",
        "late",
        "late cbs",
        "overruns",
        "worst cb µs",
        "host µs/cb",
        "worst host µs"
    );
    for block in [128u32, 256, 512] {
        for per_instance in [false, true] {
            let run = measure(per_instance, block, seconds, engine_like, render_load);
            println!(
                "{block:>6} {:>12} {:>9} {:>8} {:>10} {:>9} {:>13} {:>13} {:>13}",
                if per_instance {
                    "per plug-in"
                } else {
                    "shared"
                },
                run.callbacks,
                run.late,
                run.late_callbacks,
                run.overruns,
                run.worst_callback_us,
                run.mean_host_period_us(),
                run.worst_host_period_us
            );
        }
    }
    println!();
}
