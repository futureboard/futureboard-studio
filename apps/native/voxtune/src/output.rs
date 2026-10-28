//! The default output device, rendering the Voxtune engine.
//!
//! The engine runs at the device's own rate (decoded takes are resampled to
//! it on load), so the callback is a straight render: no resampler, no
//! queue. It never blocks and never allocates — `Shared::render_realtime`
//! try-locks the engine and plays silence for a block if the UI holds it,
//! and the scratch buffer is on the stack.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use sphere_voxtune::engine::CantoEngine;
use sphere_voxtune::shared::Shared;

/// Frames rendered per engine call; device buffers larger than this are
/// rendered in pieces.
const CHUNK_FRAMES: usize = 512;

pub struct Output {
    _stream: cpal::Stream,
    pub device_name: String,
    pub sample_rate: u32,
    /// Set by the stream's error callback (device lost).
    pub failed: Arc<AtomicBool>,
}

impl Output {
    /// Opens the default output and an engine running at its rate.
    pub fn open() -> Result<(Output, Arc<Shared>), String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("No audio output device is available.")?;
        let device_name = device.name().unwrap_or_else(|_| "Output".into());
        let config = device
            .default_output_config()
            .map_err(|e| format!("The output device has no usable format: {e}"))?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let format = config.sample_format();
        let stream_config: cpal::StreamConfig = config.into();

        let shared = Arc::new(Shared::new(CantoEngine::new(sample_rate as f32)));
        let failed = Arc::new(AtomicBool::new(false));
        let on_error = {
            let failed = failed.clone();
            move |_err: cpal::StreamError| failed.store(true, Ordering::Relaxed)
        };

        let stream = match format {
            cpal::SampleFormat::F32 => {
                let shared = shared.clone();
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [f32], _| fill(&shared, data, channels, |s| s),
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let shared = shared.clone();
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i16], _| {
                        fill(&shared, data, channels, |s| {
                            (s.clamp(-1.0, 1.0) * 32767.0) as i16
                        })
                    },
                    on_error,
                    None,
                )
            }
            cpal::SampleFormat::I32 => {
                let shared = shared.clone();
                device.build_output_stream(
                    &stream_config,
                    move |data: &mut [i32], _| {
                        fill(&shared, data, channels, |s| {
                            (s.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0) as i32
                        })
                    },
                    on_error,
                    None,
                )
            }
            other => return Err(format!("Unsupported output sample format {other:?}.")),
        }
        .map_err(|e| format!("The output stream could not be opened: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("The output stream could not start: {e}"))?;
        Ok((
            Output {
                _stream: stream,
                device_name,
                sample_rate,
                failed,
            },
            shared,
        ))
    }
}

/// Renders the engine into an interleaved device buffer of `channels`.
fn fill<T: Copy + Default>(
    shared: &Shared,
    data: &mut [T],
    channels: usize,
    convert: impl Fn(f32) -> T,
) {
    let channels = channels.max(1);
    let mut scratch = [0.0f32; CHUNK_FRAMES * 2];
    for block in data.chunks_mut(CHUNK_FRAMES * channels) {
        let frames = block.len() / channels;
        let stereo = &mut scratch[..frames * 2];
        shared.render_realtime(stereo);
        for (f, out) in block.chunks_exact_mut(channels).enumerate() {
            let (l, r) = (stereo[f * 2], stereo[f * 2 + 1]);
            match channels {
                1 => out[0] = convert((l + r) * 0.5),
                _ => {
                    out[0] = convert(l);
                    out[1] = convert(r);
                    for extra in &mut out[2..] {
                        *extra = T::default();
                    }
                }
            }
        }
    }
}
