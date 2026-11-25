//! The 3D Room Preview's scene: an orbit camera around the listener, the room
//! as a box, its speakers, the listener's head, and every placed channel —
//! built as lit triangles for the wgpu renderer in [`gpu`], plus the screen
//! points the window labels and picks with.
//!
//! Room coordinates ([`RoomPosition`]: `x` left→right, `y` back→front, `z`
//! ear level→ceiling, walls at ±1) map to the world as `X = x`, `Y = z ×`
//! [`CEILING_Y`] (up), `Z = −y` (the front wall is at `−Z`, away from a
//! camera that starts behind the listener). The floor sits below the ears at
//! [`FLOOR_Y`], so a channel at ear level floats above its own shadow.
//!
//! Everything here is plain arithmetic, so the projection that draws a
//! channel is the same one that picks it and drags it.

#[cfg(feature = "gpu-renderer")]
pub mod gpu;

use solfege_spatialaudio::{RoomPosition, SourceParams, SpatialFormat};

pub type Vec3 = [f32; 3];
/// Column-major: `m[column][row]`, as WGSL's `mat4x4<f32>` reads it.
pub type Mat4 = [[f32; 4]; 4];

/// World height of the floor, below the ear plane (`Y = 0`).
pub const FLOOR_Y: f32 = -0.45;
/// World height of the ceiling (`z = 1`).
pub const CEILING_Y: f32 = 0.9;
/// A channel's sphere radius, in world units (the walls are 2 apart).
pub const SOURCE_RADIUS: f32 = 0.065;
const FOV_Y: f32 = 0.785; // 45°

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn normalize(a: Vec3) -> Vec3 {
    let len = dot(a, a).sqrt();
    if len > 1.0e-9 {
        scale(a, 1.0 / len)
    } else {
        [0.0, 0.0, 0.0]
    }
}

fn mat_mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0f32; 4]; 4];
    for (c, column) in out.iter_mut().enumerate() {
        for (r, cell) in column.iter_mut().enumerate() {
            *cell = (0..4).map(|k| a[k][r] * b[c][k]).sum();
        }
    }
    out
}

/// Right-handed perspective onto wgpu's `0..1` depth range.
fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fov_y * 0.5).tan();
    let range = near - far;
    [
        [f / aspect.max(1.0e-3), 0.0, 0.0, 0.0],
        [0.0, f, 0.0, 0.0],
        [0.0, 0.0, far / range, -1.0],
        [0.0, 0.0, near * far / range, 0.0],
    ]
}

fn look_at(eye: Vec3, target: Vec3, up: Vec3) -> Mat4 {
    let f = normalize(sub(target, eye));
    let s = normalize(cross(f, up));
    let u = cross(s, f);
    [
        [s[0], u[0], -f[0], 0.0],
        [s[1], u[1], -f[1], 0.0],
        [s[2], u[2], -f[2], 0.0],
        [-dot(s, eye), -dot(u, eye), dot(f, eye), 1.0],
    ]
}

/// Where a room point is in the world.
pub fn world(position: RoomPosition) -> Vec3 {
    [position.x, position.z * CEILING_Y, -position.y]
}

/// A camera circling the listener. `yaw` turns it around the room (`0` is
/// behind the listener, looking at the front wall), `pitch` raises it, and
/// `distance` is how far out it sits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrbitCamera {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

/// The views a click away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraPreset {
    /// Behind and above the listener, a little to the side.
    Perspective,
    /// Straight down, front wall at the top.
    Top,
    /// From behind the listener at ear height, looking at the front wall.
    Front,
}

impl CameraPreset {
    pub const ALL: [CameraPreset; 3] = [Self::Perspective, Self::Top, Self::Front];

    pub fn label(self) -> &'static str {
        match self {
            Self::Perspective => "3D",
            Self::Top => "Top",
            Self::Front => "Front",
        }
    }
}

const TARGET: Vec3 = [0.0, 0.1, 0.0];
const MIN_PITCH: f32 = -0.25;
const MAX_PITCH: f32 = 1.55;
const MIN_DISTANCE: f32 = 1.6;
const MAX_DISTANCE: f32 = 9.0;

impl Default for OrbitCamera {
    fn default() -> Self {
        Self::preset(CameraPreset::Perspective)
    }
}

impl OrbitCamera {
    pub fn preset(preset: CameraPreset) -> Self {
        match preset {
            CameraPreset::Perspective => Self {
                yaw: 0.42,
                pitch: 0.62,
                distance: 4.1,
            },
            CameraPreset::Top => Self {
                yaw: 0.0,
                pitch: MAX_PITCH,
                distance: 4.4,
            },
            CameraPreset::Front => Self {
                yaw: 0.0,
                pitch: 0.12,
                distance: 4.2,
            },
        }
    }

    /// The preset this camera is at, if it has not been moved off one.
    pub fn at_preset(&self) -> Option<CameraPreset> {
        CameraPreset::ALL
            .into_iter()
            .find(|preset| Self::preset(*preset) == *self)
    }

    pub fn eye(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        add(TARGET, scale([sy * cp, sp, cy * cp], self.distance))
    }

    /// Turn around the room by a pointer move of `dx`, `dy` pixels.
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * 0.008;
        self.pitch = (self.pitch + dy * 0.008).clamp(MIN_PITCH, MAX_PITCH);
    }

    /// Move in (`steps > 0`) or out.
    pub fn zoom(&mut self, steps: f32) {
        self.distance = (self.distance * 0.9f32.powf(steps)).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// Up for the view: world up, except looking straight down, where the
    /// front wall is "up" on screen.
    fn up(&self) -> Vec3 {
        if self.pitch > 1.5 {
            let (sy, cy) = self.yaw.sin_cos();
            [-sy, 0.0, -cy]
        } else {
            [0.0, 1.0, 0.0]
        }
    }

    pub fn view_proj(&self, aspect: f32) -> Mat4 {
        let projection = perspective(FOV_Y, aspect, 0.05, 50.0);
        let view = look_at(self.eye(), TARGET, self.up());
        mat_mul(&projection, &view)
    }

    /// Where `point` lands in a `width` × `height` view, in pixels from its
    /// top-left, and its depth; `None` behind the camera.
    pub fn project(&self, point: Vec3, width: f32, height: f32) -> Option<(f32, f32, f32)> {
        let m = self.view_proj(width / height.max(1.0));
        let clip: Vec<f32> = (0..4)
            .map(|r| m[0][r] * point[0] + m[1][r] * point[1] + m[2][r] * point[2] + m[3][r])
            .collect();
        if clip[3] <= 1.0e-4 {
            return None;
        }
        let (x, y, z) = (clip[0] / clip[3], clip[1] / clip[3], clip[2] / clip[3]);
        Some(((x + 1.0) * 0.5 * width, (1.0 - y) * 0.5 * height, z))
    }

    /// The ray from the eye through pixel (`px`, `py`) of the view.
    pub fn ray(&self, px: f32, py: f32, width: f32, height: f32) -> (Vec3, Vec3) {
        let eye = self.eye();
        let f = normalize(sub(TARGET, eye));
        let s = normalize(cross(f, self.up()));
        let u = cross(s, f);
        let tan = (FOV_Y * 0.5).tan();
        let aspect = width / height.max(1.0);
        let nx = px / width.max(1.0) * 2.0 - 1.0;
        let ny = 1.0 - py / height.max(1.0) * 2.0;
        let dir = normalize(add(f, add(scale(s, nx * tan * aspect), scale(u, ny * tan))));
        (eye, dir)
    }

    /// The room point under pixel (`px`, `py`) at height `z` (room units),
    /// clamped inside the walls; `None` when the ray runs along that plane
    /// or away from it.
    pub fn room_at(
        &self,
        px: f32,
        py: f32,
        width: f32,
        height: f32,
        z: f32,
    ) -> Option<RoomPosition> {
        let (origin, dir) = self.ray(px, py, width, height);
        let plane_y = z * CEILING_Y;
        if dir[1].abs() < 1.0e-4 {
            return None;
        }
        let t = (plane_y - origin[1]) / dir[1];
        if t <= 0.0 {
            return None;
        }
        let hit = add(origin, scale(dir, t));
        Some(RoomPosition::new(hit[0], -hit[2], z))
    }
}

/// One vertex: position, normal (all zero for an unlit line), colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshVertex {
    pub position: Vec3,
    pub normal: Vec3,
    pub color: [f32; 4],
}

/// Floats per [`MeshVertex`] on the GPU.
pub const MESH_VERTEX_FLOATS: usize = 10;

/// A frame's triangles: opaque ones drawn first with depth writes, then the
/// translucent ones back to front.
#[derive(Debug, Clone, Default)]
pub struct RoomMesh {
    pub clear: [f32; 4],
    pub opaque: Vec<MeshVertex>,
    pub translucent: Vec<MeshVertex>,
}

/// The theme colours the scene is drawn in, as linear `0..1` RGBA.
#[derive(Debug, Clone, Copy)]
pub struct RoomPalette {
    pub background: [f32; 4],
    pub floor: [f32; 4],
    pub grid: [f32; 4],
    pub edge: [f32; 4],
    pub ear_plane: [f32; 4],
    pub head: [f32; 4],
    pub speaker: [f32; 4],
    pub accent: [f32; 4],
    pub shadow: [f32; 4],
}

/// A placed channel as the scene draws it.
#[derive(Debug, Clone, Copy)]
pub struct SceneSource {
    pub heard: SourceParams,
    pub base: SourceParams,
    pub color: [f32; 4],
    pub automated: bool,
    pub selected: bool,
}

/// A speaker and its level colour (already mixed from the meter).
#[derive(Debug, Clone, Copy)]
pub struct SceneSpeaker {
    pub position: RoomPosition,
    pub color: [f32; 4],
    pub top: bool,
}

fn with_alpha(color: [f32; 4], alpha: f32) -> [f32; 4] {
    [color[0], color[1], color[2], alpha]
}

fn push_triangle(
    out: &mut Vec<MeshVertex>,
    a: Vec3,
    b: Vec3,
    c: Vec3,
    normal: Vec3,
    color: [f32; 4],
) {
    for position in [a, b, c] {
        out.push(MeshVertex {
            position,
            normal,
            color,
        });
    }
}

/// A box with lit faces.
fn push_box(out: &mut Vec<MeshVertex>, centre: Vec3, half: Vec3, color: [f32; 4]) {
    let [cx, cy, cz] = centre;
    let [hx, hy, hz] = half;
    let corner = |sx: f32, sy: f32, sz: f32| [cx + sx * hx, cy + sy * hy, cz + sz * hz];
    // (normal, four corners counter-clockwise seen from outside)
    let faces: [(Vec3, [Vec3; 4]); 6] = [
        (
            [1.0, 0.0, 0.0],
            [
                corner(1., -1., 1.),
                corner(1., -1., -1.),
                corner(1., 1., -1.),
                corner(1., 1., 1.),
            ],
        ),
        (
            [-1.0, 0.0, 0.0],
            [
                corner(-1., -1., -1.),
                corner(-1., -1., 1.),
                corner(-1., 1., 1.),
                corner(-1., 1., -1.),
            ],
        ),
        (
            [0.0, 1.0, 0.0],
            [
                corner(-1., 1., 1.),
                corner(1., 1., 1.),
                corner(1., 1., -1.),
                corner(-1., 1., -1.),
            ],
        ),
        (
            [0.0, -1.0, 0.0],
            [
                corner(-1., -1., -1.),
                corner(1., -1., -1.),
                corner(1., -1., 1.),
                corner(-1., -1., 1.),
            ],
        ),
        (
            [0.0, 0.0, 1.0],
            [
                corner(-1., -1., 1.),
                corner(1., -1., 1.),
                corner(1., 1., 1.),
                corner(-1., 1., 1.),
            ],
        ),
        (
            [0.0, 0.0, -1.0],
            [
                corner(1., -1., -1.),
                corner(-1., -1., -1.),
                corner(-1., 1., -1.),
                corner(1., 1., -1.),
            ],
        ),
    ];
    for (normal, [a, b, c, d]) in faces {
        push_triangle(out, a, b, c, normal, color);
        push_triangle(out, a, c, d, normal, color);
    }
}

/// A line drawn as a thin unlit box along `a`→`b` (any direction).
fn push_line(out: &mut Vec<MeshVertex>, a: Vec3, b: Vec3, width: f32, color: [f32; 4]) {
    let dir = sub(b, a);
    let len = dot(dir, dir).sqrt();
    if len < 1.0e-6 {
        return;
    }
    let d = scale(dir, 1.0 / len);
    let helper = if d[1].abs() < 0.9 {
        [0.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0]
    };
    let p = scale(normalize(cross(d, helper)), width * 0.5);
    let q = scale(normalize(cross(d, p)), width * 0.5);
    let none = [0.0, 0.0, 0.0];
    let ring = |base: Vec3| {
        [
            add(base, add(p, q)),
            add(base, sub(p, q)),
            sub(base, add(p, q)),
            sub(base, sub(p, q)),
        ]
    };
    let (ra, rb) = (ring(a), ring(b));
    for i in 0..4 {
        let j = (i + 1) % 4;
        push_triangle(out, ra[i], ra[j], rb[j], none, color);
        push_triangle(out, ra[i], rb[j], rb[i], none, color);
    }
}

/// A lit UV sphere.
fn push_sphere(out: &mut Vec<MeshVertex>, centre: Vec3, radius: f32, color: [f32; 4]) {
    const RINGS: usize = 10;
    const SEGMENTS: usize = 18;
    let point = |ring: usize, segment: usize| -> Vec3 {
        let theta = std::f32::consts::PI * ring as f32 / RINGS as f32;
        let phi = std::f32::consts::TAU * segment as f32 / SEGMENTS as f32;
        [
            theta.sin() * phi.cos(),
            theta.cos(),
            theta.sin() * phi.sin(),
        ]
    };
    for ring in 0..RINGS {
        for segment in 0..SEGMENTS {
            let n = [
                point(ring, segment),
                point(ring + 1, segment),
                point(ring + 1, segment + 1),
                point(ring, segment + 1),
            ];
            let p = n.map(|n| add(centre, scale(n, radius)));
            for (a, b, c) in [(0, 1, 2), (0, 2, 3)] {
                for i in [a, b, c] {
                    out.push(MeshVertex {
                        position: p[i],
                        normal: n[i],
                        color,
                    });
                }
            }
        }
    }
}

/// A flat disc lying on the plane `y`, facing up.
fn push_disc(out: &mut Vec<MeshVertex>, centre: Vec3, radius: f32, color: [f32; 4]) {
    const SEGMENTS: usize = 24;
    let up = [0.0, 0.0, 0.0];
    for i in 0..SEGMENTS {
        let a0 = std::f32::consts::TAU * i as f32 / SEGMENTS as f32;
        let a1 = std::f32::consts::TAU * (i + 1) as f32 / SEGMENTS as f32;
        push_triangle(
            out,
            centre,
            add(centre, [a1.cos() * radius, 0.0, a1.sin() * radius]),
            add(centre, [a0.cos() * radius, 0.0, a0.sin() * radius]),
            up,
            color,
        );
    }
}

/// The speakers of `format` where they stand, with their level colours.
pub fn scene_speakers(
    format: SpatialFormat,
    peaks: &[f32],
    level_color: impl Fn(f32) -> Option<[f32; 4]>,
    rest: [f32; 4],
) -> Vec<SceneSpeaker> {
    let SpatialFormat::Surround(layout) = format else {
        return Vec::new();
    };
    layout
        .speakers()
        .iter()
        .enumerate()
        .filter(|(_, speaker)| !speaker.lfe)
        .map(|(channel, speaker)| {
            let position =
                solfege_spatialaudio::speaker_position(speaker.azimuth_deg, speaker.elevation_deg);
            let peak = peaks.get(channel).copied().unwrap_or(0.0);
            SceneSpeaker {
                position,
                color: level_color(peak).unwrap_or(rest),
                top: speaker.elevation_deg >= 20.0,
            }
        })
        .collect()
}

/// Where a speaker's box sits: on its wall, pulled in so it stays inside.
pub fn speaker_world(speaker: &SceneSpeaker) -> Vec3 {
    let w = world(speaker.position);
    [w[0] * 0.95, w[1].min(CEILING_Y - 0.06), w[2] * 0.95]
}

/// Build the frame: the room, the listener, the speakers and the channels.
pub fn build_mesh(
    camera: &OrbitCamera,
    palette: &RoomPalette,
    speakers: &[SceneSpeaker],
    sources: &[SceneSource],
    binaural: bool,
) -> RoomMesh {
    let mut opaque = Vec::with_capacity(8_192);
    // Translucent pieces, each with its distance from the eye for sorting.
    let mut translucent: Vec<(f32, Vec<MeshVertex>)> = Vec::new();
    let eye = camera.eye();
    let distance = |p: Vec3| {
        let d = sub(p, eye);
        dot(d, d)
    };

    // The floor and its grid.
    let floor = [
        [-1.0, FLOOR_Y, 1.0],
        [1.0, FLOOR_Y, 1.0],
        [1.0, FLOOR_Y, -1.0],
        [-1.0, FLOOR_Y, -1.0],
    ];
    push_triangle(
        &mut opaque,
        floor[0],
        floor[1],
        floor[2],
        [0.0, 1.0, 0.0],
        palette.floor,
    );
    push_triangle(
        &mut opaque,
        floor[0],
        floor[2],
        floor[3],
        [0.0, 1.0, 0.0],
        palette.floor,
    );
    let grid_y = FLOOR_Y + 0.002;
    for i in 0..=8 {
        let t = -1.0 + i as f32 * 0.25;
        let color = if i == 4 { palette.edge } else { palette.grid };
        push_line(
            &mut opaque,
            [t, grid_y, -1.0],
            [t, grid_y, 1.0],
            0.006,
            color,
        );
        push_line(
            &mut opaque,
            [-1.0, grid_y, t],
            [1.0, grid_y, t],
            0.006,
            color,
        );
    }
    // The room's edges.
    let (lo, hi) = (FLOOR_Y, CEILING_Y);
    let corners = [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
    for k in 0..4 {
        let [x0, z0] = corners[k];
        let [x1, z1] = corners[(k + 1) % 4];
        push_line(&mut opaque, [x0, lo, z0], [x1, lo, z1], 0.008, palette.edge);
        push_line(&mut opaque, [x0, hi, z0], [x1, hi, z1], 0.008, palette.edge);
        push_line(&mut opaque, [x0, lo, z0], [x0, hi, z0], 0.008, palette.edge);
    }
    // The ear plane, where a channel at height 0 sits.
    {
        let mut plane = Vec::new();
        let y = 0.0;
        let p = [
            [-1.0, y, 1.0],
            [1.0, y, 1.0],
            [1.0, y, -1.0],
            [-1.0, y, -1.0],
        ];
        push_triangle(
            &mut plane,
            p[0],
            p[1],
            p[2],
            [0.0, 0.0, 0.0],
            palette.ear_plane,
        );
        push_triangle(
            &mut plane,
            p[0],
            p[2],
            p[3],
            [0.0, 0.0, 0.0],
            palette.ear_plane,
        );
        translucent.push((distance([0.0, 0.0, 0.0]) + 50.0, plane));
    }

    // The listener: a head facing the front wall (−Z), ears marked for
    // headphones.
    push_sphere(&mut opaque, [0.0, 0.0, 0.0], 0.1, palette.head);
    push_box(
        &mut opaque,
        [0.0, -0.01, -0.11],
        [0.022, 0.03, 0.03],
        palette.head,
    );
    if binaural {
        for side in [-1.0f32, 1.0] {
            push_sphere(&mut opaque, [side * 0.105, 0.0, 0.0], 0.03, palette.accent);
        }
    }

    // The speakers, lit by their level; the top ring as hollow frames.
    for speaker in speakers {
        let at = speaker_world(speaker);
        if speaker.top {
            let h = 0.045;
            let pts = [
                add(at, [-h, 0.0, -h]),
                add(at, [h, 0.0, -h]),
                add(at, [h, 0.0, h]),
                add(at, [-h, 0.0, h]),
            ];
            for k in 0..4 {
                push_line(&mut opaque, pts[k], pts[(k + 1) % 4], 0.016, speaker.color);
            }
            push_box(&mut opaque, at, [0.02, 0.02, 0.02], speaker.color);
        } else {
            push_box(&mut opaque, at, [0.05, 0.065, 0.05], speaker.color);
        }
    }

    // The channels: shadow on the floor, a stem down to it, the sphere (its
    // two sides for a stereo pair), the spread as a halo, the selection as a
    // ring of light, and — while automation moves it — a ghost where its own
    // placement is.
    for source in sources {
        let heard = source.heard;
        let at = world(heard.position);
        let mut shadow = Vec::new();
        push_disc(
            &mut shadow,
            [at[0], FLOOR_Y + 0.004, at[2]],
            SOURCE_RADIUS * 1.2,
            palette.shadow,
        );
        push_line(
            &mut shadow,
            [at[0], FLOOR_Y + 0.004, at[2]],
            at,
            0.006,
            with_alpha(source.color, 0.45),
        );
        translucent.push((distance([at[0], FLOOR_Y, at[2]]), shadow));

        push_sphere(&mut opaque, at, SOURCE_RADIUS, source.color);
        let half_width = heard.half_width_radians();
        if half_width > 1.0e-3 {
            for angle in [-half_width, half_width] {
                let side = world(heard.position.rotated(angle));
                push_sphere(&mut opaque, side, SOURCE_RADIUS * 0.45, source.color);
                push_line(&mut opaque, at, side, 0.005, with_alpha(source.color, 1.0));
            }
        }
        if heard.spread > 0.01 {
            let mut halo = Vec::new();
            push_sphere(
                &mut halo,
                at,
                SOURCE_RADIUS + 0.32 * heard.spread,
                with_alpha(source.color, 0.13),
            );
            translucent.push((distance(at), halo));
        }
        if source.selected {
            let mut glow = Vec::new();
            push_sphere(
                &mut glow,
                at,
                SOURCE_RADIUS * 1.7,
                with_alpha(palette.accent, 0.28),
            );
            translucent.push((distance(at) - 1.0e-3, glow));
        }
        if source.automated {
            let ghost_at = world(source.base.position);
            let mut ghost = Vec::new();
            push_sphere(
                &mut ghost,
                ghost_at,
                SOURCE_RADIUS,
                with_alpha(source.color, 0.3),
            );
            translucent.push((distance(ghost_at), ghost));
        }
    }

    // Back to front.
    translucent.sort_by(|a, b| b.0.total_cmp(&a.0));
    RoomMesh {
        clear: palette.background,
        opaque,
        translucent: translucent.into_iter().flat_map(|(_, v)| v).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_projects_where_its_ray_comes_back_from() {
        let camera = OrbitCamera::default();
        let (w, h) = (800.0, 600.0);
        for position in [
            RoomPosition::new(0.4, -0.3, 0.0),
            RoomPosition::new(-0.9, 0.9, 0.0),
            RoomPosition::new(0.0, 0.0, 0.0),
        ] {
            let (px, py, depth) = camera.project(world(position), w, h).expect("in front");
            assert!((0.0..=1.0).contains(&depth));
            let back = camera.room_at(px, py, w, h, 0.0).expect("on the ear plane");
            assert!(
                (back.x - position.x).abs() < 1.0e-3,
                "{back:?} vs {position:?}"
            );
            assert!(
                (back.y - position.y).abs() < 1.0e-3,
                "{back:?} vs {position:?}"
            );
        }
    }

    #[test]
    fn the_default_view_puts_the_front_wall_above_the_back_wall() {
        let camera = OrbitCamera::default();
        let front = camera
            .project(world(RoomPosition::new(0.0, 1.0, 0.0)), 800.0, 600.0)
            .unwrap();
        let back = camera
            .project(world(RoomPosition::new(0.0, -1.0, 0.0)), 800.0, 600.0)
            .unwrap();
        assert!(
            front.1 < back.1,
            "front at y {} should be above back at y {}",
            front.1,
            back.1
        );
        // The back wall is nearer the camera.
        assert!(back.2 < front.2);
    }

    #[test]
    fn the_top_view_has_the_front_wall_up_and_the_right_wall_right() {
        let camera = OrbitCamera::preset(CameraPreset::Top);
        let front = camera
            .project(world(RoomPosition::new(0.0, 1.0, 0.0)), 600.0, 600.0)
            .unwrap();
        let right = camera
            .project(world(RoomPosition::new(1.0, 0.0, 0.0)), 600.0, 600.0)
            .unwrap();
        assert!(front.1 < 300.0);
        assert!(right.0 > 300.0);
    }

    #[test]
    fn orbiting_and_zooming_stay_in_range() {
        let mut camera = OrbitCamera::default();
        camera.orbit(0.0, 10_000.0);
        assert!(camera.pitch <= MAX_PITCH);
        camera.orbit(0.0, -10_000.0);
        assert!(camera.pitch >= MIN_PITCH);
        camera.zoom(100.0);
        assert_eq!(camera.distance, MIN_DISTANCE);
        camera.zoom(-100.0);
        assert_eq!(camera.distance, MAX_DISTANCE);
        assert_eq!(camera.at_preset(), None);
        assert_eq!(
            OrbitCamera::preset(CameraPreset::Top).at_preset(),
            Some(CameraPreset::Top)
        );
    }

    #[test]
    fn the_mesh_holds_whole_triangles_with_translucency_apart() {
        let palette = RoomPalette {
            background: [0.0, 0.0, 0.0, 1.0],
            floor: [0.1, 0.1, 0.1, 1.0],
            grid: [0.2, 0.2, 0.2, 1.0],
            edge: [0.3, 0.3, 0.3, 1.0],
            ear_plane: [1.0, 1.0, 1.0, 0.04],
            head: [0.5, 0.5, 0.5, 1.0],
            speaker: [0.4, 0.4, 0.4, 1.0],
            accent: [0.2, 0.8, 0.9, 1.0],
            shadow: [0.0, 0.0, 0.0, 0.35],
        };
        let source = SceneSource {
            heard: SourceParams {
                spread: 0.5,
                ..SourceParams::default()
            },
            base: SourceParams::default(),
            color: [1.0, 0.0, 0.0, 1.0],
            automated: true,
            selected: true,
        };
        let speakers = scene_speakers(
            SpatialFormat::Surround(solfege_spatialaudio::SpeakerLayout::Surround714),
            &[],
            |_| None,
            palette.speaker,
        );
        assert_eq!(speakers.len(), 11, "7.1.4 without its LFE");
        let mesh = build_mesh(
            &OrbitCamera::default(),
            &palette,
            &speakers,
            &[source],
            false,
        );
        assert_eq!(mesh.opaque.len() % 3, 0);
        assert_eq!(mesh.translucent.len() % 3, 0);
        assert!(mesh
            .opaque
            .iter()
            .all(|v| v.color[3] >= 0.99 || v.normal == [0.0; 3]));
        assert!(!mesh.translucent.is_empty());
    }
}
