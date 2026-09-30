//! Square-room position → one gain per speaker.
//!
//! **On the wall** a source is panned pairwise between the two speakers either
//! side of its direction, with 2-D VBAP (Pulkki, 1997): the gains that
//! reconstruct the direction from the two speakers' unit vectors, normalised
//! to constant power. A source at a speaker plays from that speaker alone.
//!
//! **Moving in** from the walls, the source spreads: its pairwise gains are
//! blended, in power, with an even distribution over the whole ring, by how
//! far it is from the wall in the room's square metric. At the centre every
//! speaker carries it equally — the listener is surrounded, not panned. A
//! channel's own spread control widens it further at any position. Every
//! blend keeps the summed power at one, so moving a source never changes its
//! loudness.
//!
//! **Height** layouts hold a second ring. The source's height crossfades,
//! constant-power, from the floor ring to the top ring, each panned the same
//! way on its own speakers.

use crate::layout::{MAX_CHANNELS, SpeakerLayout};
use crate::position::{RoomPosition, SourceParams};

/// Degrees an elevated speaker must be above the ear plane to be in the top
/// ring.
const TOP_RING_MIN_ELEVATION: f32 = 20.0;

#[derive(Debug, Clone, Copy)]
struct Ring {
    /// Channel index of each speaker, sorted by azimuth.
    channels: [usize; MAX_CHANNELS],
    /// Azimuth of each speaker, radians, ascending in `-π..=π`.
    azimuths: [f32; MAX_CHANNELS],
    len: usize,
    /// Whether the speakers surround the listener. A front-only arc (stereo,
    /// LCR) is not a ring: a source behind is folded onto the front.
    surrounds: bool,
}

impl Ring {
    fn of(layout: SpeakerLayout, top: bool) -> Option<Self> {
        let mut ring = Self {
            channels: [0; MAX_CHANNELS],
            azimuths: [0.0; MAX_CHANNELS],
            len: 0,
            surrounds: false,
        };
        for (channel, speaker) in layout.speakers().iter().enumerate() {
            if speaker.lfe || (speaker.elevation_deg >= TOP_RING_MIN_ELEVATION) != top {
                continue;
            }
            ring.channels[ring.len] = channel;
            ring.azimuths[ring.len] = speaker.azimuth_deg.to_radians();
            ring.len += 1;
        }
        if ring.len == 0 {
            return None;
        }
        // Insertion sort by azimuth: a handful of speakers, once.
        for i in 1..ring.len {
            let mut j = i;
            while j > 0 && ring.azimuths[j - 1] > ring.azimuths[j] {
                ring.azimuths.swap(j - 1, j);
                ring.channels.swap(j - 1, j);
                j -= 1;
            }
        }
        // A ring surrounds the listener when no gap between neighbouring
        // speakers (wrapping round the back) exceeds 180°.
        let mut widest_gap = 0.0f32;
        for i in 0..ring.len {
            let next = if i + 1 < ring.len {
                ring.azimuths[i + 1]
            } else {
                ring.azimuths[0] + std::f32::consts::TAU
            };
            widest_gap = widest_gap.max(next - ring.azimuths[i]);
        }
        ring.surrounds = ring.len >= 3 && widest_gap < std::f32::consts::PI;
        Some(ring)
    }

    /// Point-source (VBAP) gains for `azimuth`, one per ring speaker, power 1.
    fn pairwise(&self, azimuth: f32, out: &mut [f32; MAX_CHANNELS]) {
        out.fill(0.0);
        if self.len == 1 {
            out[0] = 1.0;
            return;
        }
        let mut azimuth = wrap_pi(azimuth);
        if !self.surrounds {
            // Fold the rear onto the front arc, then hold it inside the arc.
            if azimuth > std::f32::consts::FRAC_PI_2 {
                azimuth = std::f32::consts::PI - azimuth;
            } else if azimuth < -std::f32::consts::FRAC_PI_2 {
                azimuth = -std::f32::consts::PI - azimuth;
            }
            azimuth = azimuth.clamp(self.azimuths[0], self.azimuths[self.len - 1]);
        }
        // The pair either side of the direction: the last speaker at or
        // before it, and the one after (wrapping behind the listener).
        let mut lo = self.len - 1;
        for i in 0..self.len {
            if self.azimuths[i] <= azimuth {
                lo = i;
            }
        }
        if azimuth < self.azimuths[0] {
            lo = self.len - 1;
        }
        let hi = if lo + 1 < self.len { lo + 1 } else { 0 };
        if !self.surrounds && hi == 0 {
            // Past the last speaker of an arc: that speaker alone.
            out[lo] = 1.0;
            return;
        }
        let (g_lo, g_hi) = vbap_pair(self.azimuths[lo], self.azimuths[hi], azimuth);
        out[lo] = g_lo;
        out[hi] = g_hi;
    }
}

/// Wrap to `-π..=π`.
fn wrap_pi(angle: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let mut a = (angle + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI;
    if a < -std::f32::consts::PI {
        a += tau;
    }
    a
}

/// VBAP gains of a direction between two speakers, normalised to power 1.
fn vbap_pair(a1: f32, a2: f32, azimuth: f32) -> (f32, f32) {
    let (p1x, p1y) = (a1.sin(), a1.cos());
    let (p2x, p2y) = (a2.sin(), a2.cos());
    let (sx, sy) = (azimuth.sin(), azimuth.cos());
    let det = p1x * p2y - p2x * p1y;
    if det.abs() < 1.0e-6 {
        return (
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        );
    }
    let g1 = ((sx * p2y - sy * p2x) / det).max(0.0);
    let g2 = ((p1x * sy - p1y * sx) / det).max(0.0);
    let norm = (g1 * g1 + g2 * g2).sqrt();
    if norm < 1.0e-9 {
        (
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        )
    } else {
        (g1 / norm, g2 / norm)
    }
}

/// Speaker gains for a layout.
#[derive(Debug, Clone, Copy)]
pub struct SurroundPanner {
    layout: SpeakerLayout,
    floor: Option<Ring>,
    top: Option<Ring>,
}

impl SurroundPanner {
    pub fn new(layout: SpeakerLayout) -> Self {
        Self {
            layout,
            floor: Ring::of(layout, false),
            top: Ring::of(layout, true),
        }
    }

    pub fn layout(&self) -> SpeakerLayout {
        self.layout
    }

    /// One gain per channel of the layout for a point source at `params`,
    /// summing to power 1. The LFE channel is left at zero: it is a send, set
    /// by the caller from [`SourceParams::lfe`].
    pub fn gains(&self, params: &SourceParams, out: &mut [f32; MAX_CHANNELS]) {
        out.fill(0.0);
        let position = params.position.clamped();
        let (floor_share, top_share) = match (self.floor.as_ref(), self.top.as_ref()) {
            (Some(_), Some(_)) => {
                let angle = position.z * std::f32::consts::FRAC_PI_2;
                (angle.cos(), angle.sin())
            }
            (Some(_), None) => (1.0, 0.0),
            (None, Some(_)) => (0.0, 1.0),
            (None, None) => return,
        };
        for (ring, share) in [(self.floor, floor_share), (self.top, top_share)] {
            let Some(ring) = ring else {
                continue;
            };
            if share <= 1.0e-6 {
                continue;
            }
            let mut ring_gains = [0.0f32; MAX_CHANNELS];
            ring_spread_gains(&ring, position, params.spread, &mut ring_gains);
            for i in 0..ring.len {
                out[ring.channels[i]] += ring_gains[i] * share;
            }
        }
    }
}

/// Pairwise gains blended toward an even spread by distance from the wall
/// and by the channel's spread, in power.
fn ring_spread_gains(
    ring: &Ring,
    position: RoomPosition,
    spread: f32,
    out: &mut [f32; MAX_CHANNELS],
) {
    let mut point = [0.0f32; MAX_CHANNELS];
    ring.pairwise(position.azimuth(), &mut point);
    // `w` is the share of power spread evenly: all of it at the centre, none
    // of it on the wall unless the channel asks for spread.
    let radius = position.wall_radius();
    let w = (1.0 - radius * (1.0 - spread.clamp(0.0, 1.0))).clamp(0.0, 1.0);
    let even = w / ring.len as f32;
    for i in 0..ring.len {
        out[i] = ((1.0 - w) * point[i] * point[i] + even).sqrt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gains_at(layout: SpeakerLayout, x: f32, y: f32, z: f32, spread: f32) -> [f32; MAX_CHANNELS] {
        let mut out = [0.0; MAX_CHANNELS];
        SurroundPanner::new(layout).gains(
            &SourceParams {
                position: RoomPosition::new(x, y, z),
                spread,
                ..SourceParams::default()
            },
            &mut out,
        );
        out
    }

    fn power(g: &[f32; MAX_CHANNELS]) -> f32 {
        g.iter().map(|g| g * g).sum()
    }

    #[test]
    fn a_source_at_a_speaker_plays_from_that_speaker_alone() {
        // 5.1: channels L R C LFE Ls Rs. Straight ahead is the centre speaker.
        let front = gains_at(SpeakerLayout::Surround51, 0.0, 1.0, 0.0, 0.0);
        assert!((front[2] - 1.0).abs() < 1.0e-4, "{front:?}");
        assert!(front.iter().enumerate().all(|(i, g)| i == 2 || *g < 1.0e-4));
        // The right-front speaker at +30°: on the front wall at x = tan 30°.
        let right = gains_at(
            SpeakerLayout::Surround51,
            30f32.to_radians().tan(),
            1.0,
            0.0,
            0.0,
        );
        assert!((right[1] - 1.0).abs() < 1.0e-3, "{right:?}");
    }

    #[test]
    fn between_two_speakers_only_those_two_play() {
        // Hard right in 7.1 is the right side speaker (Rss, channel 7).
        let right = gains_at(SpeakerLayout::Surround71, 1.0, 0.0, 0.0, 0.0);
        assert!((right[7] - 1.0).abs() < 1.0e-3, "{right:?}");
        // Right-rear corner in 5.1: between R (+30°) … no — between Rs (+110°)
        // and Ls (-110°) only for directions behind; at 135° it is Rs and Ls.
        let corner = gains_at(SpeakerLayout::Surround51, 1.0, -1.0, 0.0, 0.0);
        let playing: Vec<usize> = (0..6).filter(|i| corner[*i] > 1.0e-3).collect();
        assert_eq!(playing, vec![4, 5], "{corner:?}");
        assert!(corner[5] > corner[4]);
    }

    #[test]
    fn panning_never_changes_the_power() {
        for layout in [
            SpeakerLayout::Quad,
            SpeakerLayout::Surround51,
            SpeakerLayout::Surround71,
            SpeakerLayout::Surround714,
            SpeakerLayout::Lcr,
        ] {
            for &(x, y, z, spread) in &[
                (0.0, 1.0, 0.0, 0.0),
                (0.7, -0.2, 0.0, 0.0),
                (-1.0, -1.0, 0.0, 0.3),
                (0.0, 0.0, 0.0, 0.0),
                (0.4, 0.4, 0.8, 0.5),
                (-0.9, 0.1, 1.0, 1.0),
            ] {
                let p = power(&gains_at(layout, x, y, z, spread));
                assert!(
                    (p - 1.0).abs() < 1.0e-4,
                    "{layout:?} at {x},{y},{z}: power {p}"
                );
            }
        }
    }

    #[test]
    fn the_centre_of_the_room_surrounds_the_listener() {
        let centre = gains_at(SpeakerLayout::Surround51, 0.0, 0.0, 0.0, 0.0);
        let expected = (1.0f32 / 5.0).sqrt();
        for channel in [0, 1, 2, 4, 5] {
            assert!((centre[channel] - expected).abs() < 1.0e-4, "{centre:?}");
        }
        assert_eq!(centre[3], 0.0, "the LFE is a send, not a speaker");
    }

    #[test]
    fn height_moves_the_source_to_the_top_ring() {
        let floor = gains_at(SpeakerLayout::Surround714, 0.0, 1.0, 0.0, 0.0);
        assert!(floor[8..12].iter().all(|g| *g < 1.0e-4));
        let ceiling = gains_at(SpeakerLayout::Surround714, 0.0, 1.0, 1.0, 0.0);
        assert!(ceiling[..8].iter().all(|g| *g < 1.0e-4), "{ceiling:?}");
        // Straight ahead on the top ring: between Ltf (-45°) and Rtf (+45°).
        assert!((ceiling[8] - ceiling[9]).abs() < 1.0e-4 && ceiling[8] > 0.6);
    }

    #[test]
    fn a_stereo_arc_folds_the_rear_onto_the_front() {
        let behind_right = gains_at(SpeakerLayout::Lcr, 0.5, -1.0, 0.0, 0.0);
        let front_right = gains_at(SpeakerLayout::Lcr, 0.5, 1.0, 0.0, 0.0);
        for i in 0..3 {
            assert!((behind_right[i] - front_right[i]).abs() < 1.0e-4);
        }
    }
}
