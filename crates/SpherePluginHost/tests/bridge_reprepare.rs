//! A bridged plug-in across a change of stream (#436), against a real spawned
//! `FutureboardPluginHostX64`:
//!
//!   cargo test -p sphere-plugin-host --features plugin-host-bin --test bridge_reprepare
//!
//! What is real here: the host process, its audio producer thread, the named
//! shared-memory region, the engine's own realtime sink ([`SharedRegionSink`])
//! and the plug-in's DSP. What is simulated: the audio device. This test plays
//! the engine callback's part of each exchange — read the block the host
//! finished, hand it the next one, ask for it — and, unlike the callback, waits
//! for the host to answer, so every measurement is exact rather than subject
//! to scheduling.
//!
//! The plug-in is the built-in limiter, because its latency is its lookahead
//! in *milliseconds*: the same setting is a different number of samples at
//! each rate, which is exactly what a plug-in left at the old rate gets wrong.
#![cfg(feature = "plugin-host-bin")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use burnlimit::ipc::{BurnLimitState, LOOKAHEAD_INDEX};
use DirectAudio::plugin_bridge::SharedPluginBridgeSink;
use SpherePluginHost::audio_bridge::SharedAudioRegion;
use SpherePluginHost::ipc::HostEvent;
use SpherePluginHost::plugin_bridge_sink::SharedRegionSink;
use SpherePluginHost::plugin_host_client::{ClientEvent, PluginHostClient};

const INSTANCE: &str = "reprepare-limiter";
/// The edit made after the load and never saved: a 5 ms lookahead, where the
/// plug-in's default is 2 ms.
const EDITED_LOOKAHEAD_MS: f32 = 5.0;
/// Below the limiter's ceiling, so it passes through delayed and untouched.
const IMPULSE: f32 = 0.25;

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

/// One loaded instance and the region it exchanges blocks through: what the
/// Studio's bridge runtime holds for an insert.
struct Bridged {
    region: Arc<SharedAudioRegion>,
    sink: SharedPluginBridgeSink,
    rate: u32,
    block: usize,
    /// Samples handed to the plug-in so far.
    fed: u64,
    /// Frames of the block the host was last asked for.
    pending: usize,
    /// Every sample it has handed back, in order.
    out: Vec<f32>,
}

impl Bridged {
    /// The Studio's load: a new region stamped with the stream's rate, mapped
    /// in the host, then the instance created at that rate with `state`.
    fn load(client: &mut PluginHostClient, rate: u32, block: usize, state: Option<String>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "Local\\FutureboardAudioBridge-{}__{INSTANCE}__t{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let region = Arc::new(
            SharedAudioRegion::create_named(&name, rate, block as u32, 2).expect("create region"),
        );
        client
            .attach_shared_audio(name, region.bytes(), INSTANCE)
            .expect("send AttachSharedAudio");
        wait_for(client, "SharedAudioAttached", |event| match event {
            HostEvent::SharedAudioAttached { attached, .. } => Some(*attached),
            _ => None,
        })
        .then_some(())
        .expect("host mapped the region");
        client
            .load_builtin_plugin(INSTANCE, "burnlimit", rate, block as u32, state)
            .expect("send LoadBuiltinPlugin");
        wait_for(client, "PluginLoaded", |event| match event {
            HostEvent::PluginLoaded { .. } => Some(()),
            HostEvent::PluginLoadFailed { error, .. } => panic!("load failed: {error}"),
            _ => None,
        });
        Self {
            sink: SharedRegionSink::into_shared(region.clone(), None),
            region,
            rate,
            block,
            fed: 0,
            pending: 0,
            out: Vec::new(),
        }
    }

    /// The Studio's unload: the instance is released and its region dropped.
    fn unload(self, client: &mut PluginHostClient) {
        client.unload_plugin(INSTANCE).expect("send UnloadPlugin");
        wait_for(client, "PluginUnloaded", |event| {
            matches!(event, HostEvent::PluginUnloaded { .. }).then_some(())
        });
    }

    /// One callback of `frames` in the engine's order — read the block the
    /// host finished, write this one, ask for it — then wait for the answer.
    fn exchange(&mut self, input: &[f32]) {
        let frames = input.len();
        // The block asked for last time, at the size it was asked for: `out`
        // stays one sample per input sample across a change of block size.
        if self.pending > 0 {
            let mut l = vec![0.0f32; self.pending];
            let mut r = vec![0.0f32; self.pending];
            let read = self.sink.read_output(&mut l, &mut r, self.pending);
            assert_eq!(read, self.pending, "the host's block for the last request");
            self.out.extend_from_slice(&l);
        }
        self.sink.write_input(input, input, frames);
        self.sink.request_block(frames as u32);
        self.fed += frames as u64;
        self.pending = frames;
        let bridge = self.region.bridge();
        let deadline = Instant::now() + Duration::from_secs(5);
        while bridge.done_seq.load(Ordering::Acquire) != bridge.request_seq.load(Ordering::Acquire)
        {
            assert!(Instant::now() < deadline, "host did not render the block");
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    fn silence(&mut self, blocks: usize) {
        let block = vec![0.0f32; self.block];
        for _ in 0..blocks {
            self.exchange(&block);
        }
    }

    /// Feed an impulse and return how many samples later it comes back: the
    /// delay the engine has to compensate for this insert.
    fn measure_delay(&mut self) -> u64 {
        // Past the 64 blocks after which the host publishes a changed latency.
        self.silence(80);
        let already = self.out.len();

        let mut block = vec![0.0f32; self.block];
        let offset = self.block / 3;
        block[offset] = IMPULSE;
        let at = self.fed + offset as u64;
        self.exchange(&block);
        self.silence(4 + 2_048 / self.block);
        let (index, peak) = self
            .out
            .iter()
            .enumerate()
            .skip(already)
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .expect("output");
        assert!(
            *peak > IMPULSE * 0.5,
            "{} Hz: the impulse did not come back (peak {peak})",
            self.rate
        );
        assert!(
            self.out[already..index].iter().all(|sample| *sample == 0.0),
            "{} Hz: something came out before the impulse did",
            self.rate
        );
        // `out[i]` is the sample the plug-in returned for input sample `i`,
        // handed over one exchange — one block — later.
        index as u64 + self.block as u64 - at
    }

    fn reported_latency(&self) -> u64 {
        u64::from(self.sink.reported_latency_samples())
    }

    fn header(&self) -> (u32, u32) {
        let bridge = self.region.bridge();
        (
            bridge.sample_rate.load(Ordering::Relaxed),
            bridge.max_block_size.load(Ordering::Relaxed),
        )
    }
}

/// The limiter's state as the Studio's mirror holds it after the unsaved edit.
fn edited_state() -> BurnLimitState {
    let mut params = burnlimit::default_params();
    params.lookahead_ms = EDITED_LOOKAHEAD_MS;
    BurnLimitState::new(params)
}

/// What the plug-in's own code says an insert in `edited_state` delays by at
/// `rate`. Never a number written here.
fn expected_latency(rate: u32) -> u64 {
    burnlimit::latency_samples_for(&edited_state().params, rate as f32) as u64
}

fn host_follows(client: &mut PluginHostClient, rate: u32, block: usize) -> bool {
    client
        .configure_audio_bridge(rate, block as u32)
        .expect("send ConfigureAudioBridge");
    wait_for(client, "AudioBridgeConfigured", |event| match event {
        HostEvent::AudioBridgeConfigured { follows_engine, .. } => Some(*follows_engine),
        _ => None,
    })
}

#[test]
fn a_bridged_plugin_follows_the_stream_once_it_is_re_prepared() {
    let mut client = PluginHostClient::spawn().expect("spawn FutureboardPluginHostX64");
    let lookahead_differs_by_rate = expected_latency(48_000) != expected_latency(44_100);
    assert!(
        lookahead_differs_by_rate,
        "the test needs a rate-bound latency"
    );

    // ── 48 kHz / 256: loaded at its defaults, then edited and never saved ──
    let mut plugin = Bridged::load(&mut client, 48_000, 256, None);
    assert!(plugin
        .sink
        .push_param(LOOKAHEAD_INDEX, EDITED_LOOKAHEAD_MS, 0));
    let delay = plugin.measure_delay();
    assert_eq!(plugin.reported_latency(), expected_latency(48_000));
    assert_eq!(delay, expected_latency(48_000) + 256, "latency + one block");
    assert!(host_follows(&mut client, 48_000, 256));

    // ── The stream reopens at 44.1 kHz / 441 and the instance is left alone:
    //    the defect. It still runs, and reports its latency, at 48 kHz. ──────
    plugin.block = 441;
    let delay = plugin.measure_delay();
    assert_eq!(plugin.header().0, 48_000, "the region still says 48 kHz");
    assert_eq!(plugin.reported_latency(), expected_latency(48_000));
    assert_eq!(delay, expected_latency(48_000) + 441);
    assert_ne!(
        plugin.reported_latency(),
        expected_latency(44_100),
        "5 ms of lookahead at the stream's rate is another number of samples"
    );
    assert!(
        !host_follows(&mut client, 44_100, 441),
        "the host says its instance does not run at the stream's rate"
    );

    // ── Re-prepared: unloaded, a new region, created again at 44.1 kHz with
    //    the state it had — the edit included. No parameter is sent again. ──
    let state = edited_state().to_json().expect("state json");
    plugin.unload(&mut client);
    let mut plugin = Bridged::load(&mut client, 44_100, 441, Some(state.clone()));
    assert_eq!(plugin.header(), (44_100, 441));
    let delay = plugin.measure_delay();
    assert_eq!(plugin.reported_latency(), expected_latency(44_100));
    assert_eq!(delay, expected_latency(44_100) + 441);
    assert!(host_follows(&mut client, 44_100, 441));

    // ── And back, at another buffer size as well. ──────────────────────────
    plugin.unload(&mut client);
    let mut plugin = Bridged::load(&mut client, 48_000, 128, Some(state));
    let delay = plugin.measure_delay();
    assert_eq!(plugin.reported_latency(), expected_latency(48_000));
    assert_eq!(delay, expected_latency(48_000) + 128);

    // ── A buffer-size change alone: the instance stays, the region's header
    //    is brought up to date, and the delay follows the block. ────────────
    plugin.region.bridge().set_max_block_size(512);
    assert_eq!(plugin.header(), (48_000, 512));
    plugin.block = 512;
    let delay = plugin.measure_delay();
    assert_eq!(plugin.reported_latency(), expected_latency(48_000));
    assert_eq!(delay, expected_latency(48_000) + 512);
    assert!(host_follows(&mut client, 48_000, 512));

    plugin.unload(&mut client);
    client.shutdown().ok();
}

/// The same lifecycle, switching back and forth: nothing drifts or leaks from
/// one instance into the next.
#[test]
fn repeated_switching_keeps_the_delay_exact() {
    let mut client = PluginHostClient::spawn().expect("spawn FutureboardPluginHostX64");
    let state = edited_state().to_json().expect("state json");
    let streams = [
        (48_000, 256),
        (44_100, 441),
        (48_000, 64),
        (44_100, 512),
        (48_000, 1024),
        (44_100, 128),
    ];
    for (rate, block) in streams {
        let mut plugin = Bridged::load(&mut client, rate, block, Some(state.clone()));
        let delay = plugin.measure_delay();
        assert_eq!(plugin.header(), (rate, block as u32));
        assert_eq!(plugin.reported_latency(), expected_latency(rate), "{rate}");
        assert_eq!(
            delay,
            expected_latency(rate) + block as u64,
            "{rate} Hz / {block}"
        );
        plugin.unload(&mut client);
    }
    client.shutdown().ok();
}

/// A third-party VST3, when one is named:
///
///   $env:FUTUREBOARD_TEST_VST3 = "C:\Program Files\Common Files\VST3\Some.vst3"
///   $env:FUTUREBOARD_TEST_VST3_CLASS = "<class id from the scanner>"
///
/// Skipped (and says so) without them: no plug-in ships with the repository.
/// It checks what can be checked of any plug-in without knowing it: the host
/// answers a prepare for another rate with the rate the instance really runs
/// at, the state captured from the 48 kHz instance is accepted by the one
/// created at 44.1 kHz, and that one reports 44.1 kHz.
#[test]
fn a_real_vst3_is_recreated_at_the_stream_rate_with_its_state() {
    let (Some(path), Some(class_id)) = (
        std::env::var_os("FUTUREBOARD_TEST_VST3"),
        std::env::var_os("FUTUREBOARD_TEST_VST3_CLASS"),
    ) else {
        eprintln!("SKIPPED: set FUTUREBOARD_TEST_VST3 and FUTUREBOARD_TEST_VST3_CLASS");
        return;
    };
    let path = path.to_string_lossy().to_string();
    let class_id = class_id.to_string_lossy().to_string();
    let mut client = PluginHostClient::spawn().expect("spawn FutureboardPluginHostX64");
    let load = |client: &mut PluginHostClient, rate: u32| {
        client
            .load_plugin(INSTANCE, &path, &class_id, rate, 256, Some("VST3".into()))
            .expect("send LoadPlugin");
        wait_for(client, "PluginLoaded", |event| match event {
            HostEvent::PluginLoaded { .. } => Some(()),
            HostEvent::PluginLoadFailed { error, .. } => panic!("load failed: {error}"),
            _ => None,
        });
    };
    let prepared_rate = |client: &mut PluginHostClient, rate: u32| {
        client
            .prepare_processing(INSTANCE, rate, 256, 2, 2)
            .expect("send PrepareProcessing");
        wait_for(client, "ProcessingPrepared", |event| match event {
            HostEvent::ProcessingPrepared { sample_rate, .. } => Some(*sample_rate),
            _ => None,
        })
    };

    load(&mut client, 48_000);
    assert_eq!(prepared_rate(&mut client, 48_000), 48_000);
    assert_eq!(
        prepared_rate(&mut client, 44_100),
        48_000,
        "a prepare for another rate is answered with the instance's real rate"
    );
    client
        .get_plugin_state(INSTANCE)
        .expect("send GetPluginState");
    let (component, controller) = wait_for(&client, "PluginState", |event| match event {
        HostEvent::PluginState {
            ok,
            component_b64,
            controller_b64,
            ..
        } => {
            assert!(*ok, "state capture failed");
            Some((component_b64.clone(), controller_b64.clone()))
        }
        _ => None,
    });
    eprintln!(
        "captured state: component {} b64 chars, controller {}",
        component.len(),
        controller.len()
    );

    client.unload_plugin(INSTANCE).expect("send UnloadPlugin");
    wait_for(&client, "PluginUnloaded", |event| {
        matches!(event, HostEvent::PluginUnloaded { .. }).then_some(())
    });
    load(&mut client, 44_100);
    assert_eq!(prepared_rate(&mut client, 44_100), 44_100);
    client
        .set_plugin_state(INSTANCE, component, controller)
        .expect("send SetPluginState");
    let restored = wait_for(&client, "PluginStateSet", |event| match event {
        HostEvent::PluginStateSet { ok, .. } => Some(*ok),
        _ => None,
    });
    assert!(restored, "the 44.1 kHz instance refused the 48 kHz state");
    client.shutdown().ok();
}
