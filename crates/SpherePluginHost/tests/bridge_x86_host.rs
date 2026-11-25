//! 32-bit plug-ins through the 32-bit bridge, end to end, against real
//! modules: the scan picks the 32-bit scanner, the plug-in loads in
//! `FutureboardPluginHostX86.exe`, audio runs through the shared region the
//! 64-bit side created, and its state comes back and goes in again.
//!
//! ```text
//! cargo build --release --target i686-pc-windows-msvc --target-dir target/x86 \
//!   -p sphere-plugin-host --features "plugin-host-bin plugin-scanner-bin" \
//!   --bin FutureboardPluginHostX64 --bin FutureboardPluginScanner
//! cargo test -p sphere-plugin-host --features plugin-host-bin --test bridge_x86_host \
//!   -- --ignored --nocapture
//! ```
//!
//! `FB_X86_PLUGINS` names the modules (`;`-separated); by default the 32-bit
//! SonoBus VST3 and VST2 where their installers put them. A module that is not
//! there is skipped; with none at all the test fails rather than pass empty.
#![cfg(all(feature = "plugin-host-bin", windows, target_pointer_width = "64"))]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use DirectAudio::plugin_bridge::SharedPluginBridgeSink;
use SpherePluginHost::audio_bridge::{bridge_kick_event_name, BridgeKickEvent, SharedAudioRegion};
use SpherePluginHost::ipc::HostEvent;
use SpherePluginHost::module_arch::{module_arch, ModuleArch};
use SpherePluginHost::plugin_bridge_sink::SharedRegionSink;
use SpherePluginHost::plugin_host_client::{ClientEvent, PluginHostClient};
use SpherePluginHost::scan::isolation::run_isolated_bundle_scan;

const RATE: u32 = 48_000;
const BLOCK: usize = 256;
const BLOCKS: usize = 400;

fn modules() -> Vec<PathBuf> {
    let listed = std::env::var("FB_X86_PLUGINS").unwrap_or_else(|_| {
        let pf86 = std::env::var("ProgramFiles(x86)")
            .unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());
        format!(r"{pf86}\Common Files\VST3\SonoBus.vst3;{pf86}\Steinberg\VSTPlugins\SonoBus.dll")
    });
    listed
        .split(';')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| {
            let there = path.exists();
            if !there {
                eprintln!("skip (not installed): {}", path.display());
            }
            there
        })
        .collect()
}

fn wait_for<T>(
    client: &PluginHostClient,
    what: &str,
    mut pick: impl FnMut(&HostEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
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

#[test]
#[ignore = "needs the 32-bit host built and a 32-bit plug-in installed"]
fn a_32_bit_plugin_scans_loads_plays_and_keeps_its_state() {
    let modules = modules();
    assert!(!modules.is_empty(), "no 32-bit plug-in to test with");
    static NEXT: AtomicU64 = AtomicU64::new(0);

    for module in modules {
        eprintln!("== {}", module.display());
        assert_eq!(module_arch(&module), ModuleArch::X86, "a 32-bit module");

        // The scan the Studio runs, which has to pick the 32-bit scanner.
        let outcome = run_isolated_bundle_scan(&module).expect("scan");
        assert!(
            outcome.failures.is_empty(),
            "scan failures: {:?}",
            outcome
                .failures
                .iter()
                .map(|failure| &failure.error)
                .collect::<Vec<_>>()
        );
        let plugin = outcome.plugins.first().expect("one plug-in in the module");
        eprintln!(
            "scanned: {} ({}) class={:?}",
            plugin.name, plugin.format, plugin.class_id
        );

        let mut client = PluginHostClient::spawn_bridge_x86().expect("spawn the 32-bit host");
        let kick_name = bridge_kick_event_name(std::process::id(), client.pid());
        let kick = Arc::new(BridgeKickEvent::create_named(&kick_name).expect("kick event"));

        let id = format!("x86-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let name = format!("Local\\FutureboardAudioBridge-{}__{id}", std::process::id());
        let region = Arc::new(
            SharedAudioRegion::create_named(&name, RATE, BLOCK as u32, 2).expect("region"),
        );
        client
            .attach_shared_audio(name, region.bytes(), &id)
            .expect("send AttachSharedAudio");
        assert!(
            wait_for(&client, "SharedAudioAttached", |event| match event {
                HostEvent::SharedAudioAttached { attached, .. } => Some(*attached),
                _ => None,
            }),
            "the 32-bit host mapped the 64-bit side's region"
        );

        client
            .load_plugin(
                &id,
                module.display().to_string(),
                plugin.class_id.clone().unwrap_or_default(),
                RATE,
                BLOCK as u32,
                Some(plugin.format.clone()),
            )
            .expect("send LoadPlugin");
        let loaded = wait_for(&client, "PluginLoaded", |event| match event {
            HostEvent::PluginLoaded { name, .. } => Some(Ok(name.clone())),
            HostEvent::PluginLoadFailed { error, .. } => Some(Err(error.clone())),
            _ => None,
        });
        let loaded = loaded.unwrap_or_else(|error| panic!("load failed: {error}"));
        eprintln!("loaded in the 32-bit host: {loaded}");
        client
            .prepare_processing(&id, RATE, BLOCK as u32, 2, 2)
            .expect("send PrepareProcessing");
        wait_for(&client, "ProcessingPrepared", |event| match event {
            HostEvent::ProcessingPrepared { .. } => Some(()),
            _ => None,
        });

        // Audio: a sine in, every block answered.
        let sink: SharedPluginBridgeSink =
            SharedRegionSink::into_shared(region.clone(), Some(kick));
        let mut phase = 0.0f32;
        let mut input = vec![0.0f32; BLOCK];
        let mut out_l = vec![0.0f32; BLOCK];
        let mut out_r = vec![0.0f32; BLOCK];
        let mut answered = 0usize;
        let mut energy = 0.0f64;
        let started = Instant::now();
        for _ in 0..BLOCKS {
            for sample in input.iter_mut() {
                *sample = (phase * std::f32::consts::TAU).sin() * 0.25;
                phase = (phase + 440.0 / RATE as f32).fract();
            }
            sink.write_input(&input, &input, BLOCK);
            sink.request_block(BLOCK as u32);
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                let got = sink.read_output(&mut out_l, &mut out_r, BLOCK);
                if got > 0 {
                    answered += 1;
                    energy += out_l
                        .iter()
                        .chain(&out_r)
                        .map(|s| f64::from(*s) * f64::from(*s))
                        .sum::<f64>();
                    break;
                }
                if Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        let rms = (energy / (answered.max(1) * BLOCK * 2) as f64).sqrt();
        eprintln!(
            "audio: {answered}/{BLOCKS} blocks answered in {:?}, output rms {rms:.4}, \
             host block {} µs (worst {} µs), latency {} samples",
            started.elapsed(),
            region.bridge().last_process_micros.load(Ordering::Relaxed),
            region.bridge().max_process_micros.load(Ordering::Relaxed),
            region.bridge().latency_samples.load(Ordering::Relaxed),
        );
        assert_eq!(
            answered, BLOCKS,
            "every block came back from the 32-bit host"
        );

        // State: out, and back in.
        client.get_plugin_state(&id).expect("send GetPluginState");
        let (component, controller) = wait_for(&client, "PluginState", |event| match event {
            HostEvent::PluginState {
                ok,
                component_b64,
                controller_b64,
                ..
            } => {
                assert!(*ok, "state captured");
                Some((component_b64.clone(), controller_b64.clone()))
            }
            _ => None,
        });
        eprintln!(
            "state: component {} b64 chars, controller {}",
            component.len(),
            controller.len()
        );
        assert!(!component.is_empty(), "the plug-in has state to keep");
        client
            .set_plugin_state(&id, component, controller)
            .expect("send SetPluginState");
        assert!(
            wait_for(&client, "PluginStateSet", |event| match event {
                HostEvent::PluginStateSet { ok, .. } => Some(*ok),
                _ => None,
            }),
            "state restored"
        );

        client.unload_plugin(&id).expect("send UnloadPlugin");
        client.shutdown().expect("send Shutdown");
        let status = client.wait_for_exit().expect("host exit");
        eprintln!("32-bit host exited: {status}");
    }
}
