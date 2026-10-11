//! DAUx Direct backend — Windows only.
//!
//! Drives the DAUx engine (`daux-sdk`) in DAUx-KS mode: the engine streams
//! straight to the audio driver's kernel pins with Kernel Streaming IOCTLs,
//! bypassing the Windows audio engine and WASAPI.
//!
//! Where the WDM-KS backend handles WaveRT pins only, DAUx-KS drives
//! *standard-streaming* pins — the packet model used by USB Audio Class
//! devices (`usbaudio.sys` / `usbaudio2.sys`). One packet is one engine
//! block, so each device period costs exactly one render callback; three
//! packets are queued (two for blocks of 5 ms and longer).
//!
//! The DAUx engine runs its own MMCSS "Pro Audio" thread and calls back into
//! [`RenderCallback`] once per block; the callback does what every backend
//! here does — `drain_commands` then `fill_output_f32` — and nothing else.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::backend::render::{drain_commands, fill_output_f32, LocalAudioState};
use crate::backend::DauxDeviceConfig;
use crate::command::EngineCommand;
use crate::engine::SharedState;
use crate::error::SphereAudioError;
use crate::runtime::RuntimeProject;

/// Handle to a running DAUx Direct stream. Dropping it stops the stream and
/// releases the kernel pin.
pub struct DauxDirectHandle {
    pub cmd_tx: Sender<EngineCommand>,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub device_name: String,
    /// What the engine negotiated, e.g. "int16 kernel streaming, 3 x 1.0 ms".
    pub output_format: String,
    /// Frames from `fill_output_f32` to the converter.
    pub output_latency_frames: u32,
    stream: daux_sdk::Stream,
    shared: Arc<SharedState>,
}

impl Drop for DauxDirectHandle {
    fn drop(&mut self) {
        self.stream.stop();
        self.shared.mmcss_active.store(false, Ordering::Relaxed);
    }
}

/// A kernel-streaming render pin the backend can open.
#[derive(Debug, Clone)]
pub struct DauxDirectDeviceInfo {
    /// `ks:<filter path>|<pin>` — pass back as the output device id.
    pub id: String,
    pub name: String,
    pub channels: u32,
    pub default_sample_rate: u32,
    /// The pin behind the default Windows playback endpoint.
    pub is_default: bool,
}

pub fn list_output_devices() -> Vec<DauxDirectDeviceInfo> {
    daux_sdk::devices(daux_sdk::Mode::Kernel, daux_sdk::Direction::Output)
        .unwrap_or_default()
        .into_iter()
        .map(|d| DauxDirectDeviceInfo {
            id: d.id,
            name: d.name,
            channels: u32::from(d.channels),
            default_sample_rate: d.default_rate,
            is_default: d.is_default,
        })
        .collect()
}

/// Runs on the DAUx engine thread, once per block.
struct RenderCallback {
    cmd_rx: Receiver<EngineCommand>,
    runtime: RuntimeProject,
    shared: Arc<SharedState>,
    local: LocalAudioState,
    sample_rate: u32,
    glitch_counter: Arc<AtomicU64>,
}

impl daux_sdk::Callback for RenderCallback {
    fn process(&mut self, info: &daux_sdk::ProcessInfo, _input: &[f32], output: &mut [f32]) {
        drain_commands(
            &self.cmd_rx,
            &mut self.runtime,
            &self.shared,
            &mut self.local,
            self.sample_rate,
        );
        // `output` is exactly one block (`frames * output_channels`), which
        // `fill_output_f32` overwrites completely.
        fill_output_f32(
            output,
            info.output_channels,
            &mut self.runtime,
            &self.shared,
            &mut self.local,
        );
    }

    fn event(&mut self, event: daux_sdk::Event) {
        self.glitch_counter.fetch_add(1, Ordering::Relaxed);
        if event == daux_sdk::Event::DeviceLost {
            // The control thread surfaces DeviceLost and runs recovery.
            self.shared.device_lost.store(true, Ordering::Relaxed);
        }
    }
}

fn open_failed(e: daux_sdk::Error) -> SphereAudioError {
    SphereAudioError::StreamOpenFailed(format!("DAUx Direct: {e}"))
}

pub fn open(
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<DauxDirectHandle, SphereAudioError> {
    let mut cfg = daux_sdk::Config::new(daux_sdk::Mode::Kernel);
    cfg.output = config.output_device_id.clone().filter(|s| !s.is_empty());
    cfg.sample_rate = config.sample_rate;
    cfg.buffer_frames = Some(
        config
            .buffer_size
            .unwrap_or(if config.safe_mode { 512 } else { 256 })
            .clamp(32, 4096),
    );
    // Resolving first fixes the rate and block size, so the render state is
    // built on the control thread and the audio thread never allocates.
    let plan = daux_sdk::plan(&cfg).map_err(open_failed)?;
    let (sample_rate, buffer_size) = (plan.sample_rate, plan.buffer_frames);

    let (tx, rx) = bounded::<EngineCommand>(512);
    let mut runtime = initial_runtime;
    runtime.retarget_sample_rate(sample_rate);
    let callback = RenderCallback {
        cmd_rx: rx,
        runtime,
        shared: Arc::clone(&shared),
        local: LocalAudioState::with_monitor_capacity(sample_rate as f64, buffer_size as usize),
        sample_rate,
        glitch_counter,
    };

    let mut stream = daux_sdk::Stream::open(plan, callback).map_err(open_failed)?;
    let info = stream.info().clone();
    shared.sample_rate.store(sample_rate, Ordering::Relaxed);
    stream.start().map_err(open_failed)?;
    // The DAUx engine thread joins MMCSS "Pro Audio" itself.
    shared.mmcss_active.store(true, Ordering::Relaxed);

    eprintln!(
        "[DAUx Direct] Stream ready: device='{}' sr={} buf={} ch={} [{}] latency={} frames",
        info.output_name,
        sample_rate,
        buffer_size,
        info.output_channels,
        info.output_format,
        info.output_latency_frames
    );

    Ok(DauxDirectHandle {
        cmd_tx: tx,
        sample_rate,
        buffer_size,
        device_name: info.output_name,
        output_format: info.output_format,
        output_latency_frames: info.output_latency_frames,
        stream,
        shared,
    })
}
