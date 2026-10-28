//! Audio file decoding, cubic resampling, waveform peak calculation, WAV export,
//! and procedural studio sample synthesis for Futureboard Studio Mobile.

use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decoded stereo audio buffer at a target sample rate.
#[derive(Debug, Clone)]
pub struct StereoAudioBuffer {
    pub sample_rate: u32,
    pub frames: usize,
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

impl StereoAudioBuffer {
    pub fn duration_seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames as f64 / self.sample_rate as f64
        }
    }

    pub fn interleaved(&self) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.frames * 2);
        for i in 0..self.frames {
            out.push(self.left[i]);
            out.push(self.right[i]);
        }
        out
    }
}

/// Decodes an audio file from disk into a `StereoAudioBuffer` resampled to `target_sample_rate`.
pub fn decode_audio_file_stereo(
    path: &Path,
    target_sample_rate: u32,
) -> Result<StereoAudioBuffer, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open '{}': {e}", path.display()))?;
    let ext = path.extension().and_then(|s| s.to_str());
    decode_media_source_stereo(Box::new(file), ext, target_sample_rate)
}

/// Decodes audio bytes in memory into a `StereoAudioBuffer` resampled to `target_sample_rate`.
pub fn decode_audio_bytes_stereo(
    bytes: &[u8],
    ext_hint: Option<&str>,
    target_sample_rate: u32,
) -> Result<StereoAudioBuffer, String> {
    if bytes.is_empty() {
        return Err("Audio byte buffer is empty".to_string());
    }
    let cursor = Cursor::new(bytes.to_vec());
    decode_media_source_stereo(Box::new(cursor), ext_hint, target_sample_rate)
}

fn decode_media_source_stereo(
    source: Box<dyn MediaSource>,
    ext_hint: Option<&str>,
    target_sample_rate: u32,
) -> Result<StereoAudioBuffer, String> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext_hint {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported or corrupted audio format: {e}"))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| "No supported audio track found".to_string())?;

    let track_id = track.id;
    let mut source_sr = track.codec_params.sample_rate.unwrap_or(0);
    let mut channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(0);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Failed to create audio decoder: {e}"))?;

    let mut left_raw = Vec::<f32>::new();
    let mut right_raw = Vec::<f32>::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(ref err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(_) => break,
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = *audio_buf.spec();
                if source_sr == 0 {
                    source_sr = spec.rate;
                }
                let ch = spec.channels.count().max(1);
                if channels == 0 {
                    channels = ch;
                }
                let duration = audio_buf.capacity() as u64;
                let buf = sample_buf
                    .get_or_insert_with(|| SampleBuffer::<f32>::new(duration, spec));
                if buf.capacity() < audio_buf.frames() * ch {
                    *buf = SampleBuffer::<f32>::new(duration, spec);
                }
                buf.copy_interleaved_ref(audio_buf);
                let interleaved = buf.samples();
                let frames = interleaved.len() / ch;
                left_raw.reserve(frames);
                right_raw.reserve(frames);
                for f in 0..frames {
                    let base = f * ch;
                    let l = interleaved[base];
                    let r = if ch >= 2 { interleaved[base + 1] } else { l };
                    left_raw.push(if l.is_finite() { l } else { 0.0 });
                    right_raw.push(if r.is_finite() { r } else { 0.0 });
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }

    if left_raw.is_empty() {
        return Err("Decoded audio contained 0 frames".to_string());
    }

    let src_sr = if source_sr == 0 {
        target_sample_rate.max(44_100)
    } else {
        source_sr
    };
    let dst_sr = if target_sample_rate == 0 {
        src_sr
    } else {
        target_sample_rate
    };

    let (left, right) = if src_sr != dst_sr {
        (
            resample_mono_cubic(&left_raw, src_sr, dst_sr),
            resample_mono_cubic(&right_raw, src_sr, dst_sr),
        )
    } else {
        (left_raw, right_raw)
    };

    let frames = left.len().min(right.len());
    Ok(StereoAudioBuffer {
        sample_rate: dst_sr,
        frames,
        left,
        right,
    })
}

/// Catmull-Rom 4-point cubic resampler.
pub fn resample_mono_cubic(input: &[f32], src_sr: u32, dst_sr: u32) -> Vec<f32> {
    if input.is_empty() || src_sr == 0 || dst_sr == 0 {
        return Vec::new();
    }
    if src_sr == dst_sr {
        return input.to_vec();
    }
    let ratio = src_sr as f64 / dst_sr as f64;
    let out_len = ((input.len() as f64) / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(out_len);
    let max_idx = input.len().saturating_sub(1);

    for i in 0..out_len {
        let pos = (i as f64) * ratio;
        let idx = pos.floor() as usize;
        let frac = (pos - (idx as f64)) as f32;

        let i0 = idx.saturating_sub(1).min(max_idx);
        let i1 = idx.min(max_idx);
        let i2 = (idx + 1).min(max_idx);
        let i3 = (idx + 2).min(max_idx);

        let y0 = input[i0];
        let y1 = input[i1];
        let y2 = input[i2];
        let y3 = input[i3];

        let c0 = y1;
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);

        let sample = ((c3 * frac + c2) * frac + c1) * frac + c0;
        output.push(sample.clamp(-1.5, 1.5));
    }
    output
}

/// Computes normalized waveform peak envelopes (`0.0..=1.0`) for UI clip rendering.
pub fn compute_stereo_peaks(left: &[f32], right: &[f32], num_bins: usize) -> Vec<f32> {
    let frames = left.len().min(right.len());
    if frames == 0 || num_bins == 0 {
        return Vec::new();
    }
    let bins = num_bins.max(1);
    let mut peaks = Vec::with_capacity(bins);
    for b in 0..bins {
        let start = (b * frames) / bins;
        let end = (((b + 1) * frames) / bins).max(start + 1).min(frames);
        let mut max_abs = 0.0f32;
        for i in start..end {
            max_abs = max_abs.max(left[i].abs()).max(right[i].abs());
        }
        peaks.push(max_abs.clamp(0.0, 1.0));
    }
    peaks
}

/// Encodes stereo f32 planar samples into a 16-bit PCM RIFF WAV byte buffer.
pub fn encode_stereo_wav_i16(left: &[f32], right: &[f32], sample_rate: u32) -> Vec<u8> {
    let frames = left.len().min(right.len());
    let data_bytes = (frames * 4) as u32;
    let riff_size = 36u32.saturating_add(data_bytes);
    let byte_rate = sample_rate.saturating_mul(4);

    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // Stereo
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes()); // Block align
    out.extend_from_slice(&16u16.to_le_bytes()); // Bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());

    for i in 0..frames {
        let l = (left[i].clamp(-1.0, 1.0) * 32767.0).round() as i16;
        let r = (right[i].clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

/// Synthesizes a 16-pad studio drum kit for `drumsampler::Dsp` so pads 0..15
/// (MIDI notes 36..51) play punchy studio percussion immediately out of the box.
pub fn populate_default_drum_kit(
    dsp: &mut drumsampler::Dsp,
    pad_loaders: &[drumsampler::PadLoader],
    sample_rate: f32,
) {
    let sr = sample_rate.max(8_000.0);
    let pad_names = [
        "Kick 808 Punch",
        "Kick Acoustic",
        "Snare Studio",
        "Clap Layered",
        "Snare Rimshot",
        "Closed Hat",
        "Pedal Hat",
        "Open Hat",
        "Tom Low",
        "Tom Mid",
        "Tom High",
        "Crash Cymbal",
        "Ride Cymbal",
        "Cowbell 808",
        "Shaker",
        "Perc FM Pluck",
    ];

    let mut params = dsp.params().clone();
    for pad_idx in 0..16.min(pad_loaders.len()) {
        params.pads[pad_idx].sample_name = Some(pad_names[pad_idx].to_string());
        // Choke group 1 for Closed/Pedal/Open Hi-Hats (pads 5, 6, 7)
        if matches!(pad_idx, 5 | 6 | 7) {
            params.pads[pad_idx].choke_group = 1;
        }
        let samples = synthesize_drum_pad_sample(pad_idx, sr);
        let frames = samples.len() / 2;
        let buf = drumsampler::PadBuffer {
            samples: samples.into_boxed_slice(),
            channels: 2,
            frames,
            sample_rate: sr,
        };
        pad_loaders[pad_idx].submit(Box::new(buf));
    }
    dsp.set_params(params);
}

fn synthesize_drum_pad_sample(pad_idx: usize, sr: f32) -> Vec<f32> {
    let tau = std::f32::consts::TAU;
    let mut rng = 0x9E37_79B9u32.wrapping_add((pad_idx as u32).wrapping_mul(0x85EB_CA6B));
    let mut next_noise = || -> f32 {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        ((rng as f32) / (u32::MAX as f32)) * 2.0 - 1.0
    };

    let duration_sec = match pad_idx {
        0 => 0.55, // Kick 808
        1 => 0.35, // Kick Acoustic
        2 => 0.32, // Snare
        3 => 0.28, // Clap
        4 => 0.16, // Rimshot
        5 | 6 => 0.11, // Closed / Pedal Hat
        7 => 0.45, // Open Hat
        8..=10 => 0.38, // Toms
        11 | 12 => 0.75, // Crash / Ride
        13 => 0.24, // Cowbell
        14 => 0.15, // Shaker
        _ => 0.22, // Perc FM
    };
    let frames = (sr * duration_sec) as usize;
    let mut out = Vec::with_capacity(frames * 2);
    let mut phase = 0.0f32;
    let mut phase2 = 0.0f32;

    for i in 0..frames {
        let t = i as f32 / sr;
        let (l, r) = match pad_idx {
            0 => {
                // 808 Sub Kick: pitch drop 155 Hz -> 46 Hz
                let f = 46.0 + 109.0 * (-t * 36.0).exp();
                phase += tau * f / sr;
                let click = (-t * 240.0).exp() * next_noise() * 0.25;
                let body = phase.sin() * (-t * 4.8).exp();
                let s = ((body + click) * 1.35).tanh() * 0.9;
                (s, s)
            }
            1 => {
                // Acoustic Kick: 180 Hz -> 56 Hz
                let f = 56.0 + 124.0 * (-t * 48.0).exp();
                phase += tau * f / sr;
                let body = phase.sin() * (-t * 9.5).exp();
                let beater = (-t * 160.0).exp() * next_noise() * 0.35;
                let s = ((body + beater) * 1.4).tanh() * 0.88;
                (s, s)
            }
            2 => {
                // Studio Snare: dual body + snappy wire noise
                let f = 185.0 + 90.0 * (-t * 65.0).exp();
                phase += tau * f / sr;
                let body = phase.sin() * (-t * 18.0).exp() * 0.65;
                let wires = next_noise() * (-t * 13.0).exp() * 0.62;
                let s = ((body + wires) * 1.25).tanh() * 0.85;
                (s, s * 0.96)
            }
            3 => {
                // Layered Clap: micro-bursts at 0, 11ms, 21ms + tail
                let burst = if t < 0.010 {
                    ((-t) * 200.0).exp()
                } else if t < 0.020 {
                    ((-(t - 0.010)) * 200.0).exp()
                } else if t < 0.030 {
                    ((-(t - 0.020)) * 200.0).exp()
                } else {
                    (-(t - 0.030) * 16.0).exp() * 0.65
                };
                let n1 = next_noise();
                let n2 = next_noise();
                ((n1 * burst * 0.82).clamp(-0.95, 0.95), (n2 * burst * 0.82).clamp(-0.95, 0.95))
            }
            4 => {
                // Rimshot
                phase += tau * 460.0 / sr;
                phase2 += tau * 1_120.0 / sr;
                let env = (-t * 42.0).exp();
                let s = ((phase.sin() * 0.6 + phase2.sin() * 0.4 + next_noise() * 0.25) * env * 1.5).tanh() * 0.8;
                (s, s)
            }
            5 | 6 => {
                // Closed / Pedal Hat: metallicinharmonic + high-frequency noise
                let decay = if pad_idx == 5 { 46.0 } else { 34.0 };
                let env = (-t * decay).exp();
                phase += tau * 6_420.0 / sr;
                phase2 += tau * 9_130.0 / sr;
                let metal = (phase.sin().signum() * 0.35 + phase2.sin().signum() * 0.35 + next_noise() * 0.65) * env * 0.55;
                (metal, metal * 0.92)
            }
            7 => {
                // Open Hat
                let env = (-t * 7.5).exp();
                phase += tau * 6_350.0 / sr;
                phase2 += tau * 8_940.0 / sr;
                let metal = (phase.sin().signum() * 0.3 + phase2.sin().signum() * 0.3 + next_noise() * 0.6) * env * 0.55;
                (metal * 0.95, metal)
            }
            8..=10 => {
                // Low / Mid / High Tom
                let base_f = match pad_idx {
                    8 => 92.0,
                    9 => 128.0,
                    _ => 172.0,
                };
                let f = base_f + base_f * 0.65 * (-t * 32.0).exp();
                phase += tau * f / sr;
                let env = (-t * 9.0).exp();
                let s = (phase.sin() * env + next_noise() * (-t * 90.0).exp() * 0.15) * 0.85;
                (s, s)
            }
            11 | 12 => {
                // Crash / Ride Cymbal
                let decay = if pad_idx == 11 { 4.2 } else { 5.5 };
                let env = (-t * decay).exp();
                phase += tau * 5_280.0 / sr;
                phase2 += tau * 7_890.0 / sr;
                let bell = if pad_idx == 12 { (tau * 2_840.0 * t).sin() * (-t * 8.0).exp() * 0.35 } else { 0.0 };
                let s = (next_noise() * 0.55 + phase.sin() * 0.22 + phase2.sin() * 0.22 + bell) * env * 0.55;
                (s, -s * 0.88)
            }
            13 => {
                // 808 Cowbell (540 Hz + 800 Hz bandpass-like square pair)
                phase += tau * 540.0 / sr;
                phase2 += tau * 800.0 / sr;
                let env = (-t * 15.0).exp();
                let s = (phase.sin().tanh() * 0.5 + phase2.sin().tanh() * 0.5) * env * 0.7;
                (s, s)
            }
            14 => {
                // Shaker
                let env = (t / 0.03).min(1.0) * (-(t - 0.03).max(0.0) * 28.0).exp();
                let n = next_noise() * env * 0.55;
                (n, n * 0.9)
            }
            _ => {
                // FM Perc Pluck
                let mod_sig = (tau * 660.0 * t).sin() * (-t * 28.0).exp() * 3.0;
                let s = (tau * 220.0 * t + mod_sig).sin() * (-t * 14.0).exp() * 0.75;
                (s, s)
            }
        };
        out.push(l.clamp(-1.0, 1.0));
        out.push(r.clamp(-1.0, 1.0));
    }
    out
}

/// Synthesizes a rich C4 (261.63 Hz) electric keys sample for `QuickSampler`.
pub fn synthesize_default_quicksampler_data(sample_rate: u32) -> Arc<quicksampler::SampleData> {
    let sr = sample_rate.max(8_000) as f32;
    let frames = (sr * 1.8) as usize;
    let f0 = 261.625_55f32; // C4
    let tau = std::f32::consts::TAU;
    let mut interleaved = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        let t = i as f32 / sr;
        let attack = (t / 0.004).min(1.0);
        let body_env = (-t * 2.2).exp();
        let tine_env = (-t * 9.0).exp();
        let fm = (tau * f0 * 2.0 * t).sin() * 1.4 * tine_env;
        let l = attack
            * ((tau * f0 * t + fm).sin() * 0.55 * body_env
                + (tau * f0 * 3.0 * t).sin() * 0.16 * tine_env
                + (tau * f0 * 4.005 * t).sin() * 0.08 * tine_env);
        let r = attack
            * ((tau * (f0 * 1.001) * t + fm).sin() * 0.55 * body_env
                + (tau * f0 * 2.998 * t).sin() * 0.16 * tine_env
                + (tau * f0 * 3.995 * t).sin() * 0.08 * tine_env);
        interleaved.push(l.clamp(-1.0, 1.0));
        interleaved.push(r.clamp(-1.0, 1.0));
    }
    Arc::new(
        quicksampler::SampleData::from_interleaved(&interleaved, 2, sample_rate.max(8_000))
            .expect("default quicksampler sample valid"),
    )
}

/// Synthesizes an 8-slice 1-bar groove loop for `Slicer`.
pub fn synthesize_default_slicer_data(sample_rate: u32) -> Arc<quicksampler::SampleData> {
    let sr = sample_rate.max(8_000) as f32;
    let slice_sec = 0.25f32; // 120 BPM eighth notes (8 slices = 2.0s)
    let slice_frames = (sr * slice_sec) as usize;
    let total_frames = slice_frames * 8;
    let freqs = [130.81f32, 164.81, 196.00, 220.00, 261.63, 293.66, 329.63, 392.00];
    let tau = std::f32::consts::TAU;
    let mut interleaved = Vec::with_capacity(total_frames * 2);

    for slice_idx in 0..8 {
        let f0 = freqs[slice_idx];
        for j in 0..slice_frames {
            let t = j as f32 / sr;
            let env = (t / 0.003).min(1.0) * (-t * 10.5).exp();
            let punch = (-t * 60.0).exp() * (tau * 90.0 * t).sin() * 0.35;
            let tone = (tau * f0 * t).sin() * 0.5 + (tau * f0 * 2.0 * t).sin() * 0.25;
            let sample = ((tone * env + punch) * 0.85).clamp(-1.0, 1.0);
            interleaved.push(sample);
            interleaved.push(sample * 0.96);
        }
    }

    Arc::new(
        quicksampler::SampleData::from_interleaved(&interleaved, 2, sample_rate.max(8_000))
            .expect("default slicer sample valid"),
    )
}

/// Synthesizes a stereo musical vocal/harmonic stem clip so the Audio track in the
/// demo project has real waveform peaks and musical audio immediately on launch.
pub fn synthesize_demo_vocal_stem(sample_rate: u32, bpm: f64, beats: f64) -> StereoAudioBuffer {
    let sr = sample_rate.max(8_000);
    let seconds = (beats * 60.0 / bpm.max(20.0)).max(0.5);
    let frames = (seconds * sr as f64) as usize;
    let mut left = vec![0.0f32; frames];
    let mut right = vec![0.0f32; frames];
    let tau = std::f32::consts::TAU;

    // 4-bar melodic phrase in C minor (C4, Eb4, G4, F4, Eb4, D4, C4, G3)
    let notes_hz = [261.63f32, 311.13, 392.00, 349.23, 311.13, 293.66, 261.63, 196.00];
    let frames_per_note = (frames / notes_hz.len()).max(1);

    for (n_idx, &hz) in notes_hz.iter().enumerate() {
        let start = n_idx * frames_per_note;
        let end = ((n_idx + 1) * frames_per_note).min(frames);
        let note_len = end.saturating_sub(start).max(1);
        for i in start..end {
            let local = i - start;
            let t = i as f32 / sr as f32;
            let local_t = local as f32 / sr as f32;
            let rel = local as f32 / note_len as f32;
            let env = (local_t / 0.025).min(1.0)
                * ((1.0 - rel) / 0.12).min(1.0)
                * (-local_t * 0.9).exp();
            let vibrato = 1.0 + 0.004 * (tau * 5.2 * t).sin() * (local_t / 0.15).min(1.0);
            let f = hz * vibrato;
            // Formant-weighted harmonic series
            let mut sig_l = 0.0f32;
            let mut sig_r = 0.0f32;
            for k in 1..=6 {
                let hf = f * k as f32;
                let formant_w = (-(hf - 750.0).powi(2) / (2.0 * 420.0f32.powi(2))).exp() * 0.9
                    + (-(hf - 2_400.0).powi(2) / (2.0 * 600.0f32.powi(2))).exp() * 0.5
                    + 0.25 / (k as f32);
                sig_l += (tau * hf * t).sin() * formant_w;
                sig_r += (tau * (hf * 1.0008) * t + 0.2 * k as f32).sin() * formant_w;
            }
            left[i] = (sig_l * env * 0.32).clamp(-1.0, 1.0);
            right[i] = (sig_r * env * 0.32).clamp(-1.0, 1.0);
        }
    }

    StereoAudioBuffer {
        sample_rate: sr,
        frames,
        left,
        right,
    }
}
