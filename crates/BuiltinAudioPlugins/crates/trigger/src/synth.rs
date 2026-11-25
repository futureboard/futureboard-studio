//! Trigger's built-in drum sounds.
//!
//! Each is synthesized once, on a control thread, into an ordinary mono WAV
//! file. The editor writes it into the plug-in's Samples folder and loads it
//! like any other sample, so a built-in sound saves, reopens and travels with
//! the project exactly as a user's own file does. Nothing here runs on the
//! audio thread.

/// The rate the sounds are rendered at; the voices resample to the host's.
pub const SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Kick,
    Snare,
    Tom,
    Clap,
}

impl Sound {
    pub const ALL: [Sound; 4] = [Sound::Kick, Sound::Snare, Sound::Tom, Sound::Clap];

    pub fn label(self) -> &'static str {
        match self {
            Sound::Kick => "Kick",
            Sound::Snare => "Snare",
            Sound::Tom => "Tom",
            Sound::Clap => "Clap",
        }
    }

    /// The file the sound is written to in the Samples folder.
    pub fn file_name(self) -> &'static str {
        match self {
            Sound::Kick => "Trigger Kick.wav",
            Sound::Snare => "Trigger Snare.wav",
            Sound::Tom => "Trigger Tom.wav",
            Sound::Clap => "Trigger Clap.wav",
        }
    }

    /// The sound, mono at [`SAMPLE_RATE`], peaking at −1 dBFS.
    pub fn render(self) -> Vec<f32> {
        let mut samples = match self {
            Sound::Kick => kick(),
            Sound::Snare => snare(),
            Sound::Tom => tom(),
            Sound::Clap => clap(),
        };
        let peak = samples.iter().fold(0.0f32, |peak, v| peak.max(v.abs()));
        if peak > 0.0 {
            let gain = 0.891 / peak;
            for v in &mut samples {
                *v *= gain;
            }
        }
        // A short fade-out, so no sound ends on a step.
        let tail = (SAMPLE_RATE as usize / 200).min(samples.len());
        let len = samples.len();
        for (i, v) in samples[len - tail..].iter_mut().enumerate() {
            *v *= 1.0 - (i + 1) as f32 / tail as f32;
        }
        samples
    }

    /// The sound as a 16-bit PCM mono WAV file.
    pub fn wav_bytes(self) -> Vec<u8> {
        wav_16bit_mono(&self.render(), SAMPLE_RATE)
    }
}

fn seconds(s: f32) -> usize {
    (s * SAMPLE_RATE as f32) as usize
}

/// A repeatable white noise.
struct Noise(u32);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 23) as f32 - 1.0
    }
}

/// A sine whose pitch falls exponentially from `from` to `to` Hz with time
/// constant `sweep` seconds, under an exponential decay of `decay` seconds.
fn swept_sine(len: f32, from: f32, to: f32, sweep: f32, decay: f32) -> Vec<f32> {
    let sr = SAMPLE_RATE as f32;
    let mut phase = 0.0f32;
    (0..seconds(len))
        .map(|i| {
            let t = i as f32 / sr;
            let hz = to + (from - to) * (-t / sweep).exp();
            phase += std::f32::consts::TAU * hz / sr;
            phase.sin() * (-t / decay).exp()
        })
        .collect()
}

fn kick() -> Vec<f32> {
    let mut body = swept_sine(0.55, 170.0, 47.0, 0.035, 0.22);
    // The beater: a few milliseconds of bright noise.
    let mut noise = Noise(0x5eed_0001);
    let sr = SAMPLE_RATE as f32;
    for (i, v) in body.iter_mut().enumerate().take(seconds(0.006)) {
        let t = i as f32 / sr;
        *v += 0.35 * noise.next() * (-t / 0.0015).exp();
    }
    body
}

fn snare() -> Vec<f32> {
    let sr = SAMPLE_RATE as f32;
    let mut noise = Noise(0x5eed_0002);
    let mut hp_prev_in = 0.0f32;
    let mut hp_prev_out = 0.0f32;
    // One-pole high-pass around 1.5 kHz for the wires.
    let a = (-std::f32::consts::TAU * 1_500.0 / sr).exp();
    let tone_a = swept_sine(0.4, 240.0, 185.0, 0.02, 0.07);
    let tone_b = swept_sine(0.4, 380.0, 330.0, 0.02, 0.05);
    (0..seconds(0.4))
        .map(|i| {
            let t = i as f32 / sr;
            let n = noise.next();
            let hp = a * (hp_prev_out + n - hp_prev_in);
            hp_prev_in = n;
            hp_prev_out = hp;
            0.55 * tone_a[i] + 0.3 * tone_b[i] + 0.8 * hp * (-t / 0.11).exp()
        })
        .collect()
}

fn tom() -> Vec<f32> {
    let mut body = swept_sine(0.7, 150.0, 98.0, 0.06, 0.25);
    let mut noise = Noise(0x5eed_0003);
    let sr = SAMPLE_RATE as f32;
    for (i, v) in body.iter_mut().enumerate().take(seconds(0.01)) {
        let t = i as f32 / sr;
        *v += 0.2 * noise.next() * (-t / 0.003).exp();
    }
    body
}

fn clap() -> Vec<f32> {
    let sr = SAMPLE_RATE as f32;
    let mut noise = Noise(0x5eed_0004);
    // A band of noise around 1.2 kHz: a high-pass then a low-pass, one pole
    // each.
    let hp_a = (-std::f32::consts::TAU * 800.0 / sr).exp();
    let lp_a = (-std::f32::consts::TAU * 2_500.0 / sr).exp();
    let (mut hp_in, mut hp_out, mut lp) = (0.0f32, 0.0f32, 0.0f32);
    // Three quick slaps, then the room.
    let slaps = [0.0f32, 0.011, 0.023];
    (0..seconds(0.45))
        .map(|i| {
            let t = i as f32 / sr;
            let n = noise.next();
            let hp = hp_a * (hp_out + n - hp_in);
            hp_in = n;
            hp_out = hp;
            lp = lp_a * lp + (1.0 - lp_a) * hp;
            let mut env = 0.0f32;
            for start in slaps {
                if t >= start {
                    env = env.max((-(t - start) / 0.006).exp());
                }
            }
            let last = slaps[slaps.len() - 1];
            if t >= last {
                env = env.max(0.6 * (-(t - last) / 0.12).exp());
            }
            lp * env * 4.0
        })
        .collect()
}

/// `samples` (−1…1) as a 16-bit PCM mono WAV file.
pub fn wav_16bit_mono(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for v in samples {
        let v = (v.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sound_is_short_loud_and_ends_in_silence() {
        for sound in Sound::ALL {
            let samples = sound.render();
            let seconds = samples.len() as f32 / SAMPLE_RATE as f32;
            assert!((0.3..=0.8).contains(&seconds), "{sound:?}: {seconds} s");
            let peak = samples.iter().fold(0.0f32, |peak, v| peak.max(v.abs()));
            assert!((peak - 0.891).abs() < 1.0e-3, "{sound:?}: peak {peak}");
            assert!(samples.iter().all(|v| v.is_finite()));
            assert_eq!(*samples.last().unwrap(), 0.0, "{sound:?} fades out");
            // The hit is at the front: the first 10 ms hold the loudest
            // sample, or close to it.
            let head = samples[..480].iter().fold(0.0f32, |p, v| p.max(v.abs()));
            assert!(head > 0.5, "{sound:?}: head {head}");
        }
    }

    #[test]
    fn the_wav_header_describes_the_data() {
        let bytes = wav_16bit_mono(&[0.0, 1.0, -1.0], 48_000);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), 32_767);
        assert_eq!(i16::from_le_bytes([bytes[48], bytes[49]]), -32_767);
        assert_eq!(bytes.len(), 44 + 6);
    }

    #[test]
    fn file_names_are_distinct_wavs() {
        for (i, a) in Sound::ALL.iter().enumerate() {
            assert!(a.file_name().ends_with(".wav"));
            for b in &Sound::ALL[i + 1..] {
                assert_ne!(a.file_name(), b.file_name());
            }
        }
    }
}
