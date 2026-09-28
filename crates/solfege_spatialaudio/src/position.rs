//! Where a source is, in the square room, and how it is spread.

use serde::{Deserialize, Serialize};

/// A point in the square room. The listener is at the centre, facing `+y`.
///
/// `x` and `y` run from wall to wall (`-1..=1`); `z` from the floor plane of
/// the ears (`0`) to the ceiling (`1`). A source on a wall is fully directed;
/// one at the centre surrounds the listener (surround) or sits inside the
/// head (binaural).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoomPosition {
    pub x: f32,
    pub y: f32,
    #[serde(default)]
    pub z: f32,
}

impl Default for RoomPosition {
    /// Front and centre, on the front wall: where a stereo mix's centre is.
    fn default() -> Self {
        Self::FRONT
    }
}

impl RoomPosition {
    pub const FRONT: Self = Self {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };

    pub const CENTRE: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }.clamped()
    }

    /// Inside the room: `x`, `y` in `-1..=1`, `z` in `0..=1`, never NaN.
    pub fn clamped(self) -> Self {
        let finite = |v: f32, lo: f32, hi: f32| if v.is_finite() { v.clamp(lo, hi) } else { 0.0 };
        Self {
            x: finite(self.x, -1.0, 1.0),
            y: finite(self.y, -1.0, 1.0),
            z: finite(self.z, 0.0, 1.0),
        }
    }

    /// Direction from the listener, radians clockwise from straight ahead
    /// (`+π/2` is hard right, `±π` straight behind). Straight ahead at the
    /// centre, where the direction is undefined.
    pub fn azimuth(self) -> f32 {
        if self.x.abs() < 1.0e-6 && self.y.abs() < 1.0e-6 {
            0.0
        } else {
            self.x.atan2(self.y)
        }
    }

    /// How far out toward the walls the source is, in the room's own square
    /// metric: `0` at the centre, `1` anywhere on a wall. This, not the
    /// Euclidean distance, is what makes the room square — a source slid
    /// along a wall stays equally "on the wall" into the corner.
    pub fn wall_radius(self) -> f32 {
        self.x.abs().max(self.y.abs()).min(1.0)
    }

    /// Straight-line distance from the listener, in room units (the walls are
    /// at 1, the corners at √2).
    pub fn distance(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Elevation above the ear plane, radians.
    pub fn elevation(self) -> f32 {
        let horizontal = (self.x * self.x + self.y * self.y).sqrt();
        self.z.atan2(horizontal.max(1.0e-6))
    }

    /// This position turned by `radians` about the listener, keeping its
    /// square radius — how a stereo source's two sides are placed around it.
    pub fn rotated(self, radians: f32) -> Self {
        let radius = self.wall_radius();
        if radius < 1.0e-6 {
            return self;
        }
        let azimuth = self.azimuth() + radians;
        // Back onto the same square: the direction scaled until its larger
        // coordinate equals the radius.
        let (dx, dy) = (azimuth.sin(), azimuth.cos());
        let scale = radius / dx.abs().max(dy.abs()).max(1.0e-6);
        Self {
            x: dx * scale,
            y: dy * scale,
            z: self.z,
        }
        .clamped()
    }
}

/// How one channel is placed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SourceParams {
    pub position: RoomPosition,
    /// `0..=1`: how far the source is spread over the other speakers (surround)
    /// or blurred (binaural) at a given position. `0` is a point.
    #[serde(default)]
    pub spread: f32,
    /// `0..=1`: how far apart a stereo source's two sides are placed around
    /// the position (`1` = ±30°, a stereo pair's own width). `0` folds the
    /// two sides to one point.
    #[serde(default = "default_width")]
    pub width: f32,
    /// `0..=1`: the LFE send, surround only.
    #[serde(default)]
    pub lfe: f32,
}

fn default_width() -> f32 {
    1.0
}

impl Default for SourceParams {
    fn default() -> Self {
        Self {
            position: RoomPosition::default(),
            spread: 0.0,
            width: 1.0,
            lfe: 0.0,
        }
    }
}

impl SourceParams {
    /// Everything in range and finite.
    pub fn sanitized(self) -> Self {
        let unit = |v: f32| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        Self {
            position: self.position.clamped(),
            spread: unit(self.spread),
            width: unit(self.width),
            lfe: unit(self.lfe),
        }
    }

    /// The angle each side of a stereo source sits off the position.
    pub fn half_width_radians(&self) -> f32 {
        self.width.clamp(0.0, 1.0) * 30.0_f32.to_radians()
    }
}

/// The room a binaural mix is heard in.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoomSettings {
    /// Half the room's width, metres: the walls at `±1` are this far from the
    /// listener. `1..=10`.
    pub half_size_m: f32,
    /// `0..=1`: how loud the walls' first reflections are. `0` is an anechoic
    /// room — precise, but the source tends to sit inside the head.
    pub reflections: f32,
}

impl Default for RoomSettings {
    fn default() -> Self {
        Self {
            half_size_m: 3.0,
            reflections: 0.35,
        }
    }
}

impl RoomSettings {
    pub fn sanitized(self) -> Self {
        Self {
            half_size_m: if self.half_size_m.is_finite() {
                self.half_size_m.clamp(1.0, 10.0)
            } else {
                3.0
            },
            reflections: if self.reflections.is_finite() {
                self.reflections.clamp(0.0, 1.0)
            } else {
                0.0
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    #[test]
    fn azimuth_is_clockwise_from_the_front() {
        assert!((RoomPosition::new(0.0, 1.0, 0.0).azimuth()).abs() < 1.0e-6);
        assert!((RoomPosition::new(1.0, 0.0, 0.0).azimuth() - FRAC_PI_2).abs() < 1.0e-6);
        assert!((RoomPosition::new(-1.0, 0.0, 0.0).azimuth() + FRAC_PI_2).abs() < 1.0e-6);
        assert!(
            (RoomPosition::new(0.0, -1.0, 0.0).azimuth().abs() - std::f32::consts::PI).abs()
                < 1.0e-6
        );
    }

    #[test]
    fn the_square_radius_makes_every_wall_point_full_radius() {
        assert_eq!(RoomPosition::new(1.0, 1.0, 0.0).wall_radius(), 1.0);
        assert_eq!(RoomPosition::new(0.3, -1.0, 0.0).wall_radius(), 1.0);
        assert_eq!(RoomPosition::new(0.25, -0.5, 0.0).wall_radius(), 0.5);
        assert_eq!(RoomPosition::CENTRE.wall_radius(), 0.0);
    }

    #[test]
    fn rotating_keeps_the_source_on_its_square() {
        let on_wall = RoomPosition::new(0.0, 1.0, 0.0).rotated(30.0_f32.to_radians());
        assert!((on_wall.wall_radius() - 1.0).abs() < 1.0e-5);
        assert!((on_wall.azimuth() - 30.0_f32.to_radians()).abs() < 1.0e-5);
        let inside = RoomPosition::new(0.5, 0.0, 0.0).rotated(-FRAC_PI_2);
        assert!((inside.wall_radius() - 0.5).abs() < 1.0e-5);
        assert!(inside.y > 0.49);
    }

    #[test]
    fn nonsense_is_clamped_not_propagated() {
        let p = RoomPosition::new(f32::NAN, 7.0, -3.0);
        assert_eq!(p, RoomPosition::new(0.0, 1.0, 0.0));
        let params = SourceParams {
            spread: f32::INFINITY,
            width: -2.0,
            lfe: 3.0,
            ..SourceParams::default()
        }
        .sanitized();
        assert_eq!((params.spread, params.width, params.lfe), (0.0, 0.0, 1.0));
    }
}
