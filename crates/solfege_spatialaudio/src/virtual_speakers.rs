//! Hearing a surround mix on two channels.

use crate::binaural::BinauralSource;
use crate::bus::SpatialBus;
use crate::layout::{MAX_CHANNELS, SpeakerLayout};
use crate::position::{RoomPosition, RoomSettings};

/// Where a speaker stands in the square room: on the wall in its direction,
/// raised by its elevation (clamped to the ceiling).
pub fn speaker_position(azimuth_deg: f32, elevation_deg: f32) -> RoomPosition {
    let azimuth = azimuth_deg.to_radians();
    let (dx, dy) = (azimuth.sin(), azimuth.cos());
    let scale = 1.0 / dx.abs().max(dy.abs()).max(1.0e-6);
    let (x, y) = (dx * scale, dy * scale);
    let horizontal = (x * x + y * y).sqrt();
    let z = (elevation_deg.to_radians().tan() * horizontal).clamp(0.0, 1.0);
    RoomPosition::new(x, y, z)
}

/// A surround bus rendered for headphones: every speaker a binaural source
/// where it stands in the room. The LFE reaches both ears.
#[derive(Debug, Clone)]
pub struct VirtualSpeakers {
    layout: SpeakerLayout,
    speakers: Vec<(usize, RoomPosition, BinauralSource)>,
    lfe: Option<usize>,
}

impl VirtualSpeakers {
    pub fn new(layout: SpeakerLayout, sample_rate: u32, max_block: usize) -> Self {
        let speakers = layout
            .speakers()
            .iter()
            .enumerate()
            .filter(|(_, speaker)| !speaker.lfe)
            .map(|(channel, speaker)| {
                (
                    channel,
                    speaker_position(speaker.azimuth_deg, speaker.elevation_deg),
                    BinauralSource::new(sample_rate, max_block),
                )
            })
            .collect();
        Self {
            layout,
            speakers,
            lfe: layout.lfe_channel(),
        }
    }

    pub fn layout(&self) -> SpeakerLayout {
        self.layout
    }

    pub fn reset(&mut self) {
        for (_, _, source) in &mut self.speakers {
            source.reset();
        }
    }

    /// Render the first `frames` of `bus` into `out_l`/`out_r` (added).
    pub fn render(
        &mut self,
        bus: &SpatialBus,
        frames: usize,
        room: &RoomSettings,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        self.render_range(bus, 0, frames, room, out_l, out_r);
    }

    /// Render `frames` of `bus` starting at `offset` into `out_l`/`out_r`
    /// (added) — for a caller working through a block in chunks. Successive
    /// ranges continue one another, as successive blocks do.
    pub fn render_range(
        &mut self,
        bus: &SpatialBus,
        offset: usize,
        frames: usize,
        room: &RoomSettings,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        let frames = frames.min(out_l.len()).min(out_r.len());
        let end = offset + frames;
        for (channel, position, source) in &mut self.speakers {
            let input = bus.channel(*channel, end);
            if input.len() < end {
                continue;
            }
            source.process_mono(
                &input[offset..end],
                *position,
                room,
                &mut out_l[..frames],
                &mut out_r[..frames],
            );
        }
        if let Some(lfe) = self.lfe {
            let input = bus.channel(lfe, end);
            if input.len() >= end {
                for (n, s) in input[offset..end].iter().enumerate() {
                    out_l[n] += s * std::f32::consts::FRAC_1_SQRT_2;
                    out_r[n] += s * std::f32::consts::FRAC_1_SQRT_2;
                }
            }
        }
    }
}

/// Fold-down coefficients `(to left, to right)` for each channel of `layout`:
/// ITU-R BS.775 — fronts at unity to their own side, the centre at −3 dB to
/// both, every surround and height speaker at −3 dB to its own side. The LFE
/// is dropped, as the recommendation does.
pub(crate) fn fold_down_coefficients(layout: SpeakerLayout) -> [(f32, f32); MAX_CHANNELS] {
    let mut out = [(0.0, 0.0); MAX_CHANNELS];
    let minus3 = std::f32::consts::FRAC_1_SQRT_2;
    for (channel, speaker) in layout.speakers().iter().enumerate() {
        if speaker.lfe {
            continue;
        }
        let az = speaker.azimuth_deg;
        let front = az.abs() <= 45.0 && speaker.elevation_deg < 20.0;
        out[channel] = if az.abs() < 1.0 {
            (minus3, minus3)
        } else {
            let level = if front { 1.0 } else { minus3 };
            if az < 0.0 { (level, 0.0) } else { (0.0, level) }
        };
    }
    out
}

/// A surround bus folded to stereo with the ITU coefficients (added into
/// `out_l`/`out_r`).
pub fn fold_down(
    bus: &SpatialBus,
    layout: SpeakerLayout,
    frames: usize,
    out_l: &mut [f32],
    out_r: &mut [f32],
) {
    fold_down_range(bus, layout, 0, frames, out_l, out_r);
}

/// [`fold_down`] of `frames` starting at `offset` in the bus.
pub fn fold_down_range(
    bus: &SpatialBus,
    layout: SpeakerLayout,
    offset: usize,
    frames: usize,
    out_l: &mut [f32],
    out_r: &mut [f32],
) {
    let frames = frames.min(out_l.len()).min(out_r.len());
    let end = offset + frames;
    let coefficients = fold_down_coefficients(layout);
    for (channel, (to_l, to_r)) in coefficients.iter().enumerate().take(layout.channel_count()) {
        if *to_l == 0.0 && *to_r == 0.0 {
            continue;
        }
        let input = bus.channel(channel, end);
        if input.len() < end {
            continue;
        }
        for (n, s) in input[offset..end].iter().enumerate() {
            out_l[n] += s * to_l;
            out_r[n] += s * to_r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speakers_stand_on_the_walls_in_their_directions() {
        let front_right = speaker_position(30.0, 0.0);
        assert!((front_right.y - 1.0).abs() < 1.0e-5);
        assert!((front_right.azimuth().to_degrees() - 30.0).abs() < 1.0e-3);
        let side = speaker_position(-90.0, 0.0);
        assert!((side.x + 1.0).abs() < 1.0e-5);
        let top = speaker_position(45.0, 45.0);
        assert!(top.z > 0.99);
    }

    #[test]
    fn the_fold_down_is_the_itu_matrix() {
        let c = fold_down_coefficients(SpeakerLayout::Surround51);
        let m3 = std::f32::consts::FRAC_1_SQRT_2;
        assert_eq!(c[0], (1.0, 0.0));
        assert_eq!(c[1], (0.0, 1.0));
        assert_eq!(c[2], (m3, m3));
        assert_eq!(c[3], (0.0, 0.0));
        assert_eq!(c[4], (m3, 0.0));
        assert_eq!(c[5], (0.0, m3));
    }

    #[test]
    fn a_left_surround_signal_is_heard_on_the_left() {
        let frames = 512;
        let layout = SpeakerLayout::Surround51;
        let mut bus = SpatialBus::new(6, frames);
        let mut phones = VirtualSpeakers::new(layout, 48_000, frames);
        let room = RoomSettings::default();
        let mut energy = (0.0f32, 0.0f32);
        let mut seed = 7u32;
        for _ in 0..20 {
            bus.clear(frames);
            for s in bus.channel_mut(4, frames) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *s = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            }
            let mut l = vec![0.0f32; frames];
            let mut r = vec![0.0f32; frames];
            phones.render(&bus, frames, &room, &mut l, &mut r);
            energy.0 += l.iter().map(|s| s * s).sum::<f32>();
            energy.1 += r.iter().map(|s| s * s).sum::<f32>();
        }
        assert!(energy.0 > energy.1 * 2.0, "{energy:?}");
    }
}
