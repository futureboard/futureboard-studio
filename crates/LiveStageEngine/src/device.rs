//! The audio interface, through cpal: WASAPI on Windows, ALSA on Linux.
//!
//! A live mixer needs every input and every output of the interface at once,
//! so streams are opened at the device's full channel count. Capture and
//! playback are two streams joined by a [`SampleRing`]; the playback callback
//! drives the mix and pulls whatever the capture side has delivered.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample, StreamConfig};
use parking_lot::Mutex;

use crate::graph::AtomicF32;
use crate::ring::SampleRing;
use crate::session::AudioSettings;

/// One interface as the user picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub channels: u16,
    pub default_sample_rate: u32,
    pub is_default: bool,
}

/// Every audio API cpal can drive here ("WASAPI", "ALSA", …).
pub fn host_names() -> Vec<String> {
    cpal::available_hosts()
        .into_iter()
        .map(|id| id.name().to_string())
        .collect()
}

fn host(name: Option<&str>) -> cpal::Host {
    name.and_then(|name| {
        cpal::available_hosts()
            .into_iter()
            .find(|id| id.name().eq_ignore_ascii_case(name))
    })
    .and_then(|id| cpal::host_from_id(id).ok())
    .unwrap_or_else(cpal::default_host)
}

/// `(inputs, outputs)` of `host` (the platform default when `None`).
pub fn list_devices(host_name: Option<&str>) -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let host = host(host_name);
    let default_in = host.default_input_device().and_then(|d| d.name().ok());
    let default_out = host.default_output_device().and_then(|d| d.name().ok());
    let describe = |device: &cpal::Device, input: bool, default: &Option<String>| {
        let name = device.name().ok()?;
        let config = if input {
            device.default_input_config().ok()?
        } else {
            device.default_output_config().ok()?
        };
        let channels = if input {
            device
                .supported_input_configs()
                .ok()
                .and_then(|configs| configs.map(|c| c.channels()).max())
        } else {
            device
                .supported_output_configs()
                .ok()
                .and_then(|configs| configs.map(|c| c.channels()).max())
        }
        .unwrap_or(config.channels());
        Some(DeviceInfo {
            is_default: default.as_deref() == Some(name.as_str()),
            name,
            channels,
            default_sample_rate: config.sample_rate().0,
        })
    };
    let inputs = host
        .input_devices()
        .map(|devices| {
            devices
                .filter_map(|d| describe(&d, true, &default_in))
                .collect()
        })
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|devices| {
            devices
                .filter_map(|d| describe(&d, false, &default_out))
                .collect()
        })
        .unwrap_or_default();
    (inputs, outputs)
}

/// Health of the running streams, shared with whoever displays it.
#[derive(Debug, Default)]
pub struct DeviceStatus {
    /// Playback callbacks that had to make up input the capture side had not
    /// delivered yet.
    pub input_underruns: AtomicU64,
    pub callbacks: AtomicU64,
    /// Share of the callback period the last mix took (0..1, can exceed 1).
    pub load: AtomicF32,
    /// Whether the audio threads run at real-time priority: [`REALTIME_UNKNOWN`]
    /// until the first callback asks, then [`REALTIME_YES`] or [`REALTIME_NO`].
    pub realtime: AtomicU8,
    pub error: Mutex<Option<String>>,
}

pub const REALTIME_UNKNOWN: u8 = 0;
pub const REALTIME_YES: u8 = 1;
pub const REALTIME_NO: u8 = 2;

/// The SCHED_FIFO priority asked for: under the kernel's own interrupt
/// threads' default (50 on PREEMPT_RT is *below* this; on a stock kernel
/// interrupts are not threads), above everything else on the machine.
#[cfg(target_os = "linux")]
const REALTIME_PRIORITY: libc::c_int = 70;

/// Puts the calling thread — an audio callback, on its first call — in the
/// real-time scheduling class, so the web server, a browser or a disk write
/// on the same machine cannot keep it waiting past its period. Linux only
/// (cpal's ALSA threads run at normal priority otherwise; WASAPI and Core
/// Audio raise their own). Needs RLIMIT_RTPRIO (the appliance's service has
/// it) or CAP_SYS_NICE; without them it is refused, and the status says so.
fn promote_to_realtime(status: &DeviceStatus) {
    #[cfg(target_os = "linux")]
    {
        // musl's sched_param has more fields than glibc's: zero them all,
        // then set the one that matters.
        // SAFETY: sched_param is plain integers; all-zero is valid.
        let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
        param.sched_priority = REALTIME_PRIORITY;
        // SAFETY: plain syscalls on this thread's own handle.
        let ok = unsafe {
            libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) == 0
        };
        // One refused thread is enough to say no.
        let state = if ok { REALTIME_YES } else { REALTIME_NO };
        let _ = status.realtime.compare_exchange(
            REALTIME_UNKNOWN,
            state,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        if !ok {
            status.realtime.store(REALTIME_NO, Ordering::Relaxed);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = status;
    }
}

/// Open streams and what they actually run at.
pub struct DeviceStreams {
    _input: Option<cpal::Stream>,
    _output: cpal::Stream,
    pub sample_rate: u32,
    pub in_channels: usize,
    pub out_channels: usize,
    pub input_name: Option<String>,
    pub output_name: String,
}

fn find_device(host: &cpal::Host, name: Option<&str>, input: bool) -> Option<cpal::Device> {
    match name {
        Some(name) => {
            let mut devices = if input {
                host.input_devices().ok()?.collect::<Vec<_>>()
            } else {
                host.output_devices().ok()?.collect::<Vec<_>>()
            };
            let index = devices
                .iter()
                .position(|d| d.name().map(|n| n == name).unwrap_or(false))?;
            Some(devices.swap_remove(index))
        }
        None if input => host.default_input_device(),
        None => host.default_output_device(),
    }
}

/// The widest configuration of `device` at `rate`, preferring float samples.
fn pick_config(
    device: &cpal::Device,
    input: bool,
    rate: u32,
) -> Result<(u16, SampleFormat), String> {
    let configs: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(|e| e.to_string())?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|e| e.to_string())?
            .collect()
    };
    configs
        .iter()
        .filter(|c| c.min_sample_rate().0 <= rate && rate <= c.max_sample_rate().0)
        .filter(|c| {
            matches!(
                c.sample_format(),
                SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32
            )
        })
        .max_by_key(|c| (c.channels(), c.sample_format() == SampleFormat::F32))
        .map(|c| (c.channels(), c.sample_format()))
        .ok_or_else(|| format!("no usable format at {rate} Hz"))
}

/// Open the interface and call `render(input, output)` for every playback
/// callback: `input` interleaved at the input's channel count, `output`
/// interleaved at the output's, both `f32`.
pub fn open(
    settings: &AudioSettings,
    status: Arc<DeviceStatus>,
    render: impl FnMut(&[f32], &mut [f32]) + Send + 'static,
) -> Result<DeviceStreams, String> {
    let host = host(settings.host.as_deref());
    let output = find_device(&host, settings.output_device.as_deref(), false)
        .ok_or_else(|| "no output device".to_string())?;
    let output_name = output.name().unwrap_or_default();
    let sample_rate = if settings.sample_rate > 0 {
        settings.sample_rate
    } else {
        output
            .default_output_config()
            .map_err(|e| e.to_string())?
            .sample_rate()
            .0
    };
    let (out_channels, out_format) =
        pick_config(&output, false, sample_rate).map_err(|e| format!("{output_name}: {e}"))?;

    // One ALSA device both ways (`hw:CARD=USB,DEV=0`): the output's handle
    // already holds its capture side open, so looking it up again finds the
    // card busy. Record through the same handle.
    let input = match settings.input_device.as_deref() {
        Some(name) if cfg!(target_os = "linux") && name == output_name => Some(output.clone()),
        name => find_device(&host, name, true),
    };
    let input_config = input
        .as_ref()
        .map(|device| pick_config(device, true, sample_rate))
        .transpose()
        .map_err(|e| format!("input: {e}"))?;
    let in_channels = input_config.map(|(c, _)| c as usize).unwrap_or(0);

    let buffer = (settings.buffer_frames > 0).then_some(settings.buffer_frames);
    // Room for a few periods of capture, plus slack for a device that
    // delivers in bigger chunks than it plays.
    let ring = Arc::new(SampleRing::new(
        in_channels.max(1) * (buffer.unwrap_or(1024) as usize * 8).max(16_384),
    ));

    let input_stream = match (&input, input_config) {
        (Some(device), Some((channels, format))) => Some(build_input(
            device,
            channels,
            format,
            sample_rate,
            buffer,
            ring.clone(),
            status.clone(),
        )?),
        _ => None,
    };
    let output_stream = build_output(
        &output,
        out_channels,
        out_format,
        sample_rate,
        buffer,
        Duplex {
            ring,
            in_channels,
            out_channels: out_channels as usize,
            input: vec![0.0; 8192 * in_channels.max(1)],
            output: vec![0.0; 8192 * out_channels as usize],
            render,
            status,
            sample_rate,
        },
    )?;
    if let Some(stream) = &input_stream {
        stream.play().map_err(|e| e.to_string())?;
    }
    output_stream.play().map_err(|e| e.to_string())?;
    Ok(DeviceStreams {
        _input: input_stream,
        _output: output_stream,
        sample_rate,
        in_channels,
        out_channels: out_channels as usize,
        input_name: input.and_then(|d| d.name().ok()),
        output_name,
    })
}

fn stream_config(channels: u16, rate: u32, buffer: Option<u32>) -> StreamConfig {
    StreamConfig {
        channels,
        sample_rate: cpal::SampleRate(rate),
        buffer_size: buffer.map_or(cpal::BufferSize::Default, cpal::BufferSize::Fixed),
    }
}

fn report(status: &Arc<DeviceStatus>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    let status = status.clone();
    move |error| {
        *status.error.lock() = Some(error.to_string());
    }
}

fn build_input(
    device: &cpal::Device,
    channels: u16,
    format: SampleFormat,
    rate: u32,
    buffer: Option<u32>,
    ring: Arc<SampleRing>,
    status: Arc<DeviceStatus>,
) -> Result<cpal::Stream, String> {
    fn typed<T: SizedSample>(
        device: &cpal::Device,
        config: &StreamConfig,
        ring: Arc<SampleRing>,
        status: &Arc<DeviceStatus>,
    ) -> Result<cpal::Stream, cpal::BuildStreamError>
    where
        f32: cpal::FromSample<T>,
    {
        let mut scratch = vec![0.0f32; 8192 * config.channels as usize];
        let promote = status.clone();
        let mut promoted = false;
        device.build_input_stream(
            config,
            move |data: &[T], _| {
                if !promoted {
                    promoted = true;
                    promote_to_realtime(&promote);
                }
                for chunk in data.chunks(scratch.len()) {
                    for (out, sample) in scratch.iter_mut().zip(chunk) {
                        *out = cpal::Sample::from_sample(*sample);
                    }
                    ring.push(&scratch[..chunk.len()]);
                }
            },
            report(status),
            None,
        )
    }
    let attempt = |buffer: Option<u32>| {
        let config = stream_config(channels, rate, buffer);
        match format {
            SampleFormat::I16 => typed::<i16>(device, &config, ring.clone(), &status),
            SampleFormat::I32 => typed::<i32>(device, &config, ring.clone(), &status),
            _ => typed::<f32>(device, &config, ring.clone(), &status),
        }
    };
    // Not every API takes a fixed period; fall back to the device's own.
    attempt(buffer)
        .or_else(|_| attempt(None))
        .map_err(|e| format!("input stream: {e}"))
}

struct Duplex<F> {
    ring: Arc<SampleRing>,
    in_channels: usize,
    out_channels: usize,
    input: Vec<f32>,
    output: Vec<f32>,
    render: F,
    status: Arc<DeviceStatus>,
    sample_rate: u32,
}

impl<F: FnMut(&[f32], &mut [f32])> Duplex<F> {
    /// Mix `frames` into `self.output`, taking the same number of frames of
    /// input from the ring.
    fn run(&mut self, frames: usize) {
        let started = Instant::now();
        let in_ch = self.in_channels;
        if in_ch > 0 {
            let available = self.ring.len() / in_ch;
            // Capture running ahead of playback (two clocks, or a burst)
            // would add latency forever; drop the excess beyond a few periods.
            let keep = frames * 3 + 256;
            if available > keep + frames {
                self.ring.skip((available - keep) * in_ch);
            }
            let wanted = frames * in_ch;
            let got = self.ring.pop(&mut self.input[..wanted]);
            if got < wanted {
                self.input[got..wanted].fill(0.0);
                self.status.input_underruns.fetch_add(1, Ordering::Relaxed);
            }
        }
        let input = &self.input[..frames * in_ch];
        let output = &mut self.output[..frames * self.out_channels];
        (self.render)(input, output);
        let period = frames as f32 / self.sample_rate.max(1) as f32;
        self.status
            .load
            .store(started.elapsed().as_secs_f32() / period.max(1e-6));
        self.status.callbacks.fetch_add(1, Ordering::Relaxed);
    }
}

fn build_output<F: FnMut(&[f32], &mut [f32]) + Send + 'static>(
    device: &cpal::Device,
    channels: u16,
    format: SampleFormat,
    rate: u32,
    buffer: Option<u32>,
    duplex: Duplex<F>,
) -> Result<cpal::Stream, String> {
    fn typed<
        T: SizedSample + cpal::FromSample<f32>,
        F: FnMut(&[f32], &mut [f32]) + Send + 'static,
    >(
        device: &cpal::Device,
        config: &StreamConfig,
        handoff: Arc<parking_lot::Mutex<Option<Duplex<F>>>>,
        status: &Arc<DeviceStatus>,
    ) -> Result<cpal::Stream, cpal::BuildStreamError> {
        let mut owned: Option<Duplex<F>> = None;
        device.build_output_stream(
            config,
            move |data: &mut [T], _| {
                // The state is handed over once, on the first callback (a
                // failed build attempt never ran one, so it never took it).
                // After that the callback owns it outright: no lock per block.
                if owned.is_none() {
                    owned = handoff.try_lock().and_then(|mut slot| slot.take());
                    if let Some(duplex) = &owned {
                        promote_to_realtime(&duplex.status);
                    }
                }
                let Some(duplex) = owned.as_mut() else {
                    data.fill(T::EQUILIBRIUM);
                    return;
                };
                let out_ch = duplex.out_channels.max(1);
                let max_frames = duplex.output.len() / out_ch;
                for chunk in data.chunks_mut(max_frames * out_ch) {
                    let frames = chunk.len() / out_ch;
                    duplex.run(frames);
                    for (out, sample) in chunk.iter_mut().zip(&duplex.output) {
                        *out = T::from_sample(*sample);
                    }
                }
            },
            report(status),
            None,
        )
    }
    let status = duplex.status.clone();
    let duplex = Arc::new(parking_lot::Mutex::new(Some(duplex)));
    let attempt = |buffer: Option<u32>| {
        let config = stream_config(channels, rate, buffer);
        match format {
            SampleFormat::I16 => typed::<i16, F>(device, &config, duplex.clone(), &status),
            SampleFormat::I32 => typed::<i32, F>(device, &config, duplex.clone(), &status),
            _ => typed::<f32, F>(device, &config, duplex.clone(), &status),
        }
    };
    attempt(buffer)
        .or_else(|_| attempt(None))
        .map_err(|e| format!("output stream: {e}"))
}
