//! The pitch-class wheel shared by Find Tempo & Key and the Chord Generator:
//! one draw-only frame description, two painters.
//!
//! Twelve pitch-class segments sit on a circle-of-fifths ring, so keys that
//! share most of their notes sit next to each other and a detected key reads as
//! one bright arc. Segment length is the measured pitch-class energy; the tonic
//! carries the accent, the rest of the scale stays neutral, and notes outside
//! the key recede. Inside the ring a bar-progress arc and a beat ripple run at
//! the detected tempo, so the number can be checked by eye against the music.
//!
//! * [`render_wgpu`] rasterises the frame with a single distance-field shader
//!   into an offscreen texture and hands the readback to GPUI as an image —
//!   the same path the controller lane uses. This is the default.
//! * [`render_gpui`] paints the same geometry with GPUI paths. It is the
//!   fallback when no WGPU adapter is available, so the wheel never goes blank.
//!
//! Neither painter reads project state; both draw exactly the [`WheelFrame`].

use std::f32::consts::TAU;
use std::hash::{Hash, Hasher};

use gpui::{
    canvas, point, px, AnyElement, Bounds, IntoElement, PathBuilder, PathStyle, Pixels, Point,
    Rgba, StrokeOptions, Styled,
};

use crate::theme::Colors;

/// Pitch class shown at ring position `i` (0 at twelve o'clock, clockwise):
/// the circle of fifths starting on C.
pub(crate) fn fifths_pitch_class(position: usize) -> usize {
    (position * 7) % 12
}

/// Ring position of pitch class `pc` — the inverse of [`fifths_pitch_class`].
pub(crate) fn fifths_position(pc: usize) -> usize {
    (pc * 7) % 12
}

/// Ring proportions as fractions of the wheel radius. Shared by both painters
/// and by the label layout in the window, so text and segments cannot drift.
pub(crate) mod ring {
    /// Inner edge of the pitch-class segments.
    pub const SEGMENT_INNER: f32 = 0.60;
    /// Outer limit a full-energy segment reaches.
    pub const SEGMENT_OUTER: f32 = 0.98;
    /// Bar-progress track.
    pub const BAR: f32 = 0.54;
    /// Centre disc edge.
    pub const DISC: f32 = 0.49;
    /// Where a beat ripple starts.
    pub const RIPPLE_START: f32 = 0.16;
    /// Radius the pitch-class labels sit on.
    pub const LABEL: f32 = 0.68;
}

/// Everything a painter needs, in logical pixels.
#[derive(Debug, Clone)]
pub(crate) struct WheelFrame {
    pub size: f32,
    /// Device scale, so the GPU painter rasterises at physical resolution.
    pub scale: f32,
    /// Displayed pitch-class energy, index 0 = C, each `0..=1`.
    pub energy: [f32; 12],
    /// Pitch classes that belong to the chosen key.
    pub in_key: [bool; 12],
    pub tonic: Option<usize>,
    /// Analysis sweep angle in radians (clockwise from twelve o'clock).
    pub sweep: Option<f32>,
    /// Position inside the current beat, `0..1`. `None` = no pulse.
    pub beat_phase: Option<f32>,
    /// Position inside a four-beat bar, `0..1`.
    pub bar_phase: f32,
}

impl WheelFrame {
    /// Stable identity of what would be drawn, so the GPU painter re-renders
    /// only when the frame actually changed.
    fn content_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.size.to_bits().hash(&mut hasher);
        self.scale.to_bits().hash(&mut hasher);
        for value in self.energy {
            value.to_bits().hash(&mut hasher);
        }
        self.in_key.hash(&mut hasher);
        self.tonic.hash(&mut hasher);
        self.sweep.map(f32::to_bits).hash(&mut hasher);
        self.beat_phase.map(f32::to_bits).hash(&mut hasher);
        self.bar_phase.to_bits().hash(&mut hasher);
        hasher.finish()
    }
}

/// Colours shared by both painters so a backend switch changes only the
/// rasteriser.
struct Palette {
    accent: Rgba,
    ink: Rgba,
}

fn palette() -> Palette {
    Palette {
        accent: Colors::accent_primary(),
        ink: Colors::text_primary(),
    }
}

/// Opacity of a segment's fill relative to the ink colour.
const IN_KEY_ALPHA: f32 = 0.55;
const OUT_OF_KEY_ALPHA: f32 = 0.18;
const TRACK_ALPHA: f32 = 0.06;
const SEGMENT_GAP_PX: f32 = 1.5;

// ── GPUI painter (fallback) ──────────────────────────────────────────────────

/// Paint the frame with GPUI paths. Used when WGPU is unavailable.
pub(crate) fn render_gpui(frame: &WheelFrame) -> AnyElement {
    let frame = frame.clone();
    canvas(
        |_bounds, _window, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let palette = palette();
            let radius = frame.size * 0.5;
            let centre = bounds.origin + point(px(radius), px(radius));
            let r_in = radius * ring::SEGMENT_INNER;
            let r_max = radius * ring::SEGMENT_OUTER;
            let step = TAU / 12.0;
            // Half the gap as an angle at the inner edge, where it is tightest.
            let gap = SEGMENT_GAP_PX / r_in;

            for position in 0..12 {
                let pc = fifths_pitch_class(position);
                let a0 = position as f32 * step + gap;
                let a1 = (position + 1) as f32 * step - gap;
                paint_annulus_sector(
                    window,
                    centre,
                    r_in,
                    r_max,
                    a0,
                    a1,
                    Colors::with_alpha(palette.ink, TRACK_ALPHA),
                );
                let energy = frame.energy[pc].clamp(0.0, 1.0);
                if energy > 0.001 {
                    let color = if frame.tonic == Some(pc) {
                        palette.accent
                    } else if frame.in_key[pc] {
                        Colors::with_alpha(palette.ink, IN_KEY_ALPHA)
                    } else {
                        Colors::with_alpha(palette.ink, OUT_OF_KEY_ALPHA)
                    };
                    let r_out = r_in + (r_max - r_in) * energy;
                    paint_annulus_sector(window, centre, r_in, r_out, a0, a1, color);
                }
            }

            if let Some(sweep) = frame.sweep {
                // The leading edge of the analysis sweep, one segment wide.
                paint_annulus_sector(
                    window,
                    centre,
                    r_in,
                    r_max,
                    sweep - step,
                    sweep,
                    Colors::with_alpha(palette.accent, 0.28),
                );
            }

            let disc = radius * ring::DISC;
            paint_arc(
                window,
                centre,
                disc,
                0.0,
                TAU,
                1.0,
                Colors::with_alpha(palette.ink, 0.12),
            );

            if let Some(beat) = frame.beat_phase {
                let bar_r = radius * ring::BAR;
                paint_arc(
                    window,
                    centre,
                    bar_r,
                    0.0,
                    TAU,
                    2.0,
                    Colors::with_alpha(palette.ink, 0.08),
                );
                if frame.bar_phase > 0.0 {
                    paint_arc(
                        window,
                        centre,
                        bar_r,
                        0.0,
                        frame.bar_phase * TAU,
                        2.0,
                        Colors::with_alpha(palette.accent, 0.85),
                    );
                }
                let fade = (1.0 - beat).powi(2);
                let ripple =
                    radius * (ring::RIPPLE_START + (ring::DISC - ring::RIPPLE_START) * beat);
                paint_arc(
                    window,
                    centre,
                    ripple,
                    0.0,
                    TAU,
                    2.0,
                    Colors::with_alpha(palette.accent, 0.6 * fade),
                );
            }
        },
    )
    .size(px(frame.size))
    .into_any_element()
}

fn polar(centre: Point<Pixels>, radius: f32, angle: f32) -> Point<Pixels> {
    // Angle 0 is twelve o'clock, increasing clockwise (screen y grows down).
    centre + point(px(radius * angle.sin()), px(-radius * angle.cos()))
}

fn paint_annulus_sector(
    window: &mut gpui::Window,
    centre: Point<Pixels>,
    r_in: f32,
    r_out: f32,
    a0: f32,
    a1: f32,
    color: Rgba,
) {
    if a1 <= a0 || r_out <= r_in {
        return;
    }
    let steps = (((a1 - a0) / TAU) * 96.0).ceil().max(2.0) as usize;
    let mut path = PathBuilder::fill();
    path.move_to(polar(centre, r_in, a0));
    for i in 0..=steps {
        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
        path.line_to(polar(centre, r_out, a));
    }
    for i in (0..=steps).rev() {
        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
        path.line_to(polar(centre, r_in, a));
    }
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_arc(
    window: &mut gpui::Window,
    centre: Point<Pixels>,
    radius: f32,
    a0: f32,
    a1: f32,
    width: f32,
    color: Rgba,
) {
    if a1 <= a0 || radius <= 0.0 || color.a <= 0.0 {
        return;
    }
    let steps = (((a1 - a0) / TAU) * 128.0).ceil().max(2.0) as usize;
    let options = StrokeOptions::default().with_line_width(width);
    let mut path = PathBuilder::stroke(px(width)).with_style(PathStyle::Stroke(options));
    path.move_to(polar(centre, radius, a0));
    for i in 1..=steps {
        let a = a0 + (a1 - a0) * i as f32 / steps as f32;
        path.line_to(polar(centre, radius, a));
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

// ── WGPU painter (offscreen + readback) ──────────────────────────────────────

/// Rasterise the frame on the GPU and present it as a GPUI image. `None` when
/// WGPU is unavailable or the pass failed; the caller then paints with
/// [`render_gpui`].
#[cfg(feature = "gpu-renderer")]
pub(crate) fn render_wgpu(frame: &WheelFrame, cx: &mut gpui::App) -> Option<AnyElement> {
    use gpui::{img, ImageSource, ObjectFit, StyledImage};
    let image = gpu::render(frame, cx)?;
    Some(
        img(ImageSource::Render(image))
            .size(px(frame.size))
            .object_fit(ObjectFit::Fill)
            .into_any_element(),
    )
}

#[cfg(not(feature = "gpu-renderer"))]
pub(crate) fn render_wgpu(_frame: &WheelFrame, _cx: &mut gpui::App) -> Option<AnyElement> {
    None
}

#[cfg(feature = "gpu-renderer")]
mod gpu {
    use std::cell::RefCell;
    use std::sync::Arc;

    use gpui::RenderImage;
    use image::{Frame, ImageBuffer};
    use smallvec::SmallVec;

    use super::{
        palette, ring, WheelFrame, IN_KEY_ALPHA, OUT_OF_KEY_ALPHA, SEGMENT_GAP_PX, TRACK_ALPHA,
    };

    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
    /// `Globals` in the shader: 64 floats, 256 bytes.
    const GLOBALS_FLOATS: usize = 64;
    const GLOBALS_SIZE: u64 = (GLOBALS_FLOATS * 4) as u64;
    /// Physical edge length above which the wheel is left to GPUI.
    const MAX_EDGE: u32 = 2048;

    /// One full-screen pass. Every pixel resolves its polar coordinate, finds
    /// its ring position on the circle of fifths, and composites track,
    /// segment, sweep, bar arc, beat dots, disc edge and ripple with analytic
    /// anti-aliasing. Colours arrive premultiplied over a transparent target.
    const SHADER: &str = r#"
struct Globals {
    // x, y = viewport (physical px), z = device scale, w = tonic (-1 = none)
    viewport: vec4<f32>,
    // x = sweep angle (-1 = none), y = beat phase (-1 = none), z = bar phase
    motion: vec4<f32>,
    accent: vec4<f32>,
    ink: vec4<f32>,
    // x = track, y = in-key, z = out-of-key alpha, w = segment gap (logical px)
    alphas: vec4<f32>,
    // x = segment inner, y = segment outer, z = bar, w = disc (fractions of R)
    rings: vec4<f32>,
    // x = ripple start fraction
    rings2: vec4<f32>,
    _pad: vec4<f32>,
    energy: array<vec4<f32>, 3>,
    in_key: array<vec4<f32>, 3>,
    _pad2: array<vec4<f32>, 2>,
};

@group(0) @binding(0) var<uniform> g: Globals;

const TAU: f32 = 6.28318530718;

@vertex
fn vs_fullscreen(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    return vec4<f32>(corners[index], 0.0, 1.0);
}

// Coverage of the band [a, b] at distance `r`, 1 px anti-aliased.
fn band(r: f32, a: f32, b: f32) -> f32 {
    return smoothstep(a - 0.5, a + 0.5, r) * (1.0 - smoothstep(b - 0.5, b + 0.5, r));
}

// Coverage of a ring of `width` centred on radius `c`.
fn stroke(r: f32, c: f32, width: f32) -> f32 {
    return 1.0 - smoothstep(width * 0.5 - 0.5, width * 0.5 + 0.5, abs(r - c));
}

fn over(dst: vec4<f32>, src: vec4<f32>) -> vec4<f32> {
    return src + dst * (1.0 - src.a);
}

@fragment
fn fs_wheel(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let scale = g.viewport.z;
    let centre = g.viewport.xy * 0.5;
    let p = pos.xy - centre;
    let radius = min(g.viewport.x, g.viewport.y) * 0.5;
    let r = length(p);

    // Clockwise from twelve o'clock.
    var ang = atan2(p.x, -p.y);
    if (ang < 0.0) { ang = ang + TAU; }
    let step = TAU / 12.0;
    let seg_f = ang / step;
    let position = u32(floor(seg_f)) % 12u;
    let local = fract(seg_f);
    let pc = (position * 7u) % 12u;

    let r_in = radius * g.rings.x;
    let r_max = radius * g.rings.y;
    // Gap between segments, constant in pixels along the arc.
    let edge_px = min(local, 1.0 - local) * step * max(r, 1.0);
    let half_gap = g.alphas.w * scale * 0.5;
    let gap = smoothstep(half_gap - 0.5, half_gap + 0.5, edge_px);

    var out = vec4<f32>(0.0);

    // Segment track.
    out = over(out, g.ink * (g.alphas.x * band(r, r_in, r_max) * gap));

    // Segment fill, a touch brighter toward its outer end.
    let energy = clamp(g.energy[pc / 4u][pc % 4u], 0.0, 1.0);
    if (energy > 0.001) {
        let r_out = r_in + (r_max - r_in) * energy;
        var tone = g.ink * g.alphas.z;
        if (g.in_key[pc / 4u][pc % 4u] > 0.5) { tone = g.ink * g.alphas.y; }
        if (i32(g.viewport.w) == i32(pc)) { tone = g.accent; }
        let lift = mix(0.78, 1.0, clamp((r - r_in) / max(r_out - r_in, 1.0), 0.0, 1.0));
        out = over(out, tone * (lift * band(r, r_in, r_out) * gap));
    }

    // Analysis sweep: a bright leading edge with a decaying trail.
    if (g.motion.x >= 0.0) {
        var behind = g.motion.x - ang;
        if (behind < 0.0) { behind = behind + TAU; }
        let trail = exp(-behind * 2.6) * band(r, r_in, r_max) * gap;
        out = over(out, g.accent * (0.42 * trail));
    }

    // Disc edge.
    let disc = radius * g.rings.w;
    out = over(out, g.ink * (0.12 * stroke(r, disc, 1.0 * scale)));

    if (g.motion.y >= 0.0) {
        let beat = clamp(g.motion.y, 0.0, 1.0);
        let bar = clamp(g.motion.z, 0.0, 1.0);
        let bar_r = radius * g.rings.z;
        let bar_line = stroke(r, bar_r, 2.0 * scale);
        out = over(out, g.ink * (0.08 * bar_line));
        // Progress through the bar, with a soft head.
        let head = bar * TAU;
        if (ang <= head) {
            let glow = mix(0.45, 0.9, clamp(1.0 - (head - ang) / TAU * 4.0, 0.0, 1.0));
            out = over(out, g.accent * (glow * bar_line));
        }
        // Four beat dots on the bar track; the current one carries the accent.
        let current = u32(floor(bar * 4.0)) % 4u;
        for (var i = 0u; i < 4u; i = i + 1u) {
            let a = f32(i) * TAU * 0.25;
            let dot_c = vec2<f32>(sin(a), -cos(a)) * bar_r;
            let d = length(p - dot_c);
            let dot_r = 3.0 * scale;
            let cover = 1.0 - smoothstep(dot_r - 0.6, dot_r + 0.6, d);
            var tone = g.ink * 0.3;
            if (i == current) {
                tone = g.accent * mix(1.0, 0.7, beat);
            }
            out = over(out, tone * cover);
        }
        // Beat ripple, expanding from near the centre to the disc edge.
        let fade = (1.0 - beat) * (1.0 - beat);
        let ripple_r = radius * mix(g.rings2.x, g.rings.w, beat);
        out = over(out, g.accent * (0.6 * fade * stroke(r, ripple_r, 2.0 * scale)));
        // Disc glow on the downbeat of each beat.
        let glow = exp(-beat * 6.0) * 0.10 * band(r, 0.0, disc);
        out = over(out, g.accent * glow);
    }

    return out;
}
"#;

    pub(crate) struct WheelGpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
        pipeline: wgpu::RenderPipeline,
        bind_group: wgpu::BindGroup,
        globals: wgpu::Buffer,
        target: Option<(wgpu::Texture, wgpu::TextureView, u32)>,
        readback: Option<(wgpu::Buffer, u64)>,
        last_hash: u64,
        last_image: Option<Arc<RenderImage>>,
    }

    thread_local! {
        /// `None` = not tried yet; `Some(None)` = unavailable on this machine.
        static WHEEL_GPU: RefCell<Option<Option<WheelGpu>>> = const { RefCell::new(None) };
    }

    fn gpu_debug() -> bool {
        std::env::var_os("FUTUREBOARD_GPU_RENDERER_DEBUG").is_some()
    }

    pub(crate) fn render(frame: &WheelFrame, cx: &mut gpui::App) -> Option<Arc<RenderImage>> {
        WHEEL_GPU.with(|cell| {
            let mut slot = cell.borrow_mut();
            let gpu = slot.get_or_insert_with(|| match WheelGpu::new() {
                Ok(gpu) => Some(gpu),
                Err(error) => {
                    eprintln!(
                        "[gpu-renderer] tempo/key wheel WGPU unavailable: {error}; using GPUI paint"
                    );
                    None
                }
            });
            let gpu = gpu.as_mut()?;
            let hash = frame.content_hash();
            if gpu.last_hash == hash {
                if let Some(image) = gpu.last_image.clone() {
                    return Some(image);
                }
            }
            match gpu.render(frame) {
                Ok(image) => {
                    // Release the previous frame from GPUI's atlas, or every
                    // animation tick would leak one texture.
                    if let Some(previous) = gpu.last_image.replace(image.clone()) {
                        cx.drop_image(previous, None);
                    }
                    gpu.last_hash = hash;
                    Some(image)
                }
                Err(error) => {
                    if gpu_debug() {
                        eprintln!("[gpu-renderer] tempo/key wheel pass failed: {error}");
                    }
                    None
                }
            }
        })
    }

    /// Release the cached frame when the window closes, so its texture does
    /// not outlive the only surface that shows it.
    pub(crate) fn release(cx: &mut gpui::App) {
        WHEEL_GPU.with(|cell| {
            if let Some(Some(gpu)) = cell.borrow_mut().as_mut() {
                if let Some(image) = gpu.last_image.take() {
                    cx.drop_image(image, None);
                }
                gpu.last_hash = 0;
            }
        });
    }

    fn premultiplied(color: gpui::Rgba) -> [f32; 4] {
        [
            color.r * color.a,
            color.g * color.a,
            color.b * color.a,
            color.a,
        ]
    }

    impl WheelGpu {
        pub(crate) fn new() -> Result<Self, String> {
            let instance = wgpu::Instance::default();
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                }))
                .map_err(|_| "no WGPU adapter".to_string())?;
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("futureboard-tempo-key-wheel"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                    memory_hints: wgpu::MemoryHints::Performance,
                    trace: wgpu::Trace::Off,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                }))
                .map_err(|error| format!("device request failed: {error}"))?;

            let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tempo-key-wheel-shader"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
            let bind_group_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("tempo-key-wheel-layout"),
                    entries: &[wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    }],
                });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tempo-key-wheel-pipeline-layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("tempo-key-wheel-pipeline"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_wheel"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            if let Some(error) = pollster::block_on(validation.pop()) {
                return Err(format!("pipeline validation failed: {error}"));
            }

            let globals = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tempo-key-wheel-globals"),
                size: GLOBALS_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tempo-key-wheel-bind-group"),
                layout: &bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                }],
            });
            Ok(Self {
                device,
                queue,
                pipeline,
                bind_group,
                globals,
                target: None,
                readback: None,
                last_hash: 0,
                last_image: None,
            })
        }

        fn ensure_target(&mut self, edge: u32) {
            if self.target.as_ref().is_some_and(|(_, _, e)| *e == edge) {
                return;
            }
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tempo-key-wheel-offscreen"),
                size: wgpu::Extent3d {
                    width: edge,
                    height: edge,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.target = Some((texture, view, edge));
        }

        fn ensure_readback(&mut self, bytes: u64) -> wgpu::Buffer {
            if self.readback.as_ref().is_none_or(|(_, size)| *size < bytes) {
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tempo-key-wheel-readback"),
                    size: bytes,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                self.readback = Some((buffer, bytes));
            }
            self.readback.as_ref().expect("readback").0.clone()
        }

        fn globals(frame: &WheelFrame, edge: u32) -> [f32; GLOBALS_FLOATS] {
            let palette = palette();
            let mut g = [0f32; GLOBALS_FLOATS];
            g[0] = edge as f32;
            g[1] = edge as f32;
            g[2] = frame.scale.max(0.5);
            g[3] = frame.tonic.map(|t| t as f32).unwrap_or(-1.0);
            g[4] = frame.sweep.unwrap_or(-1.0);
            g[5] = frame.beat_phase.unwrap_or(-1.0);
            g[6] = frame.bar_phase;
            g[8..12].copy_from_slice(&premultiplied(palette.accent));
            g[12..16].copy_from_slice(&premultiplied(palette.ink));
            g[16] = TRACK_ALPHA;
            g[17] = IN_KEY_ALPHA;
            g[18] = OUT_OF_KEY_ALPHA;
            g[19] = SEGMENT_GAP_PX * 2.0;
            g[20] = ring::SEGMENT_INNER;
            g[21] = ring::SEGMENT_OUTER;
            g[22] = ring::BAR;
            g[23] = ring::DISC;
            g[24] = ring::RIPPLE_START;
            g[32..44].copy_from_slice(&frame.energy);
            for (i, member) in frame.in_key.iter().enumerate() {
                g[44 + i] = if *member { 1.0 } else { 0.0 };
            }
            g
        }

        fn render(&mut self, frame: &WheelFrame) -> Result<Arc<RenderImage>, String> {
            let edge = (frame.size * frame.scale.max(0.5)).round().max(1.0) as u32;
            let bgra = self.render_bgra(frame)?;
            let buffer = ImageBuffer::from_raw(edge, edge, bgra)
                .ok_or_else(|| "readback size mismatch".to_string())?;
            Ok(Arc::new(RenderImage::new(SmallVec::from_elem(
                Frame::new(buffer),
                1,
            ))))
        }

        /// One pass, read back as tightly packed premultiplied BGRA rows.
        pub(crate) fn render_bgra(&mut self, frame: &WheelFrame) -> Result<Vec<u8>, String> {
            let edge = (frame.size * frame.scale.max(0.5)).round().max(1.0) as u32;
            if edge > MAX_EDGE {
                return Err("wheel too large for the GPU pass".to_string());
            }
            let globals = Self::globals(frame, edge);
            let bytes: Vec<u8> = globals.iter().flat_map(|v| v.to_ne_bytes()).collect();
            debug_assert_eq!(bytes.len() as u64, GLOBALS_SIZE);
            self.queue.write_buffer(&self.globals, 0, &bytes);

            self.ensure_target(edge);
            let unpadded = edge * 4;
            let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let padded = unpadded.div_ceil(align) * align;
            let readback_bytes = (padded * edge) as u64;
            let readback = self.ensure_readback(readback_bytes);
            let (texture, view, _) = self.target.as_ref().expect("target");

            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tempo-key-wheel"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tempo-key-wheel-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
            encoder.copy_texture_to_buffer(
                texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(edge),
                    },
                },
                wgpu::Extent3d {
                    width: edge,
                    height: edge,
                    depth_or_array_layers: 1,
                },
            );
            self.queue.submit(Some(encoder.finish()));

            let slice = readback.slice(..readback_bytes);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            self.device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .map_err(|error| format!("poll failed: {error}"))?;
            let mapped = slice.get_mapped_range();
            // GPUI wants premultiplied BGRA; the pass wrote premultiplied RGBA.
            let mut bgra = Vec::with_capacity((unpadded * edge) as usize);
            for row in 0..edge {
                let start = (row * padded) as usize;
                for px in mapped[start..start + unpadded as usize].chunks_exact(4) {
                    bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            drop(mapped);
            readback.unmap();
            Ok(bgra)
        }
    }
}

/// Drop the GPU painter's cached frame. Safe to call without the feature.
pub(crate) fn release_gpu_frame(cx: &mut gpui::App) {
    #[cfg(feature = "gpu-renderer")]
    gpu::release(cx);
    #[cfg(not(feature = "gpu-renderer"))]
    let _ = cx;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_of_fifths_order_round_trips() {
        let order: Vec<usize> = (0..12).map(fifths_pitch_class).collect();
        // C G D A E B F# C# G# D# A# F
        assert_eq!(order, vec![0, 7, 2, 9, 4, 11, 6, 1, 8, 3, 10, 5]);
        for pc in 0..12 {
            assert_eq!(fifths_pitch_class(fifths_position(pc)), pc);
        }
    }

    #[test]
    fn content_hash_changes_with_motion_only_when_it_moves() {
        let frame = WheelFrame {
            size: 280.0,
            scale: 2.0,
            energy: [0.5; 12],
            in_key: [false; 12],
            tonic: Some(9),
            sweep: None,
            beat_phase: Some(0.25),
            bar_phase: 0.1,
        };
        let same = frame.clone();
        assert_eq!(frame.content_hash(), same.content_hash());
        let mut moved = frame.clone();
        moved.beat_phase = Some(0.3);
        assert_ne!(frame.content_hash(), moved.content_hash());
    }

    /// Compiles the WGSL and renders one frame. Skips (passes) on a machine
    /// with no adapter; a shader or pipeline error fails.
    #[cfg(feature = "gpu-renderer")]
    #[test]
    fn wgpu_pass_renders_the_wheel() {
        let mut gpu = match gpu::WheelGpu::new() {
            Ok(gpu) => gpu,
            Err(error) if error.contains("adapter") || error.contains("device") => {
                eprintln!("skipping: {error}");
                return;
            }
            Err(error) => panic!("wheel pipeline failed: {error}"),
        };
        let mut energy = [0.2; 12];
        energy[9] = 1.0;
        let frame = WheelFrame {
            size: 64.0,
            scale: 1.0,
            energy,
            in_key: [true; 12],
            tonic: Some(9),
            sweep: None,
            beat_phase: Some(0.1),
            bar_phase: 0.3,
        };
        let pixels = gpu.render_bgra(&frame).expect("render");
        assert_eq!(pixels.len(), 64 * 64 * 4);
        // Transparent at the corner, painted on the ring.
        assert_eq!(pixels[3], 0, "corner must stay transparent");
        assert!(
            pixels.chunks_exact(4).any(|px| px[3] > 0),
            "wheel painted nothing"
        );
    }
}
