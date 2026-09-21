//! Controller-lane envelope drawing: one draw-only snapshot, two painters.
//!
//! The lane's gesture code hands a [`CcLaneSnapshot`] — sampled curve, point
//! handles, base value, colours — to whichever painter the user's Renderer
//! setting selects:
//!
//! * [`render_gpui`] batches everything into a single GPUI canvas (filled
//!   body, anti-aliased stroke, dashed base value, handles). This is the
//!   default and the fallback.
//! * [`render_wgpu`] rasterises the same snapshot on the GPU with a distance-
//!   field shader (exact AA line, gradient body, SDF handles) into an
//!   offscreen texture and reads it back into a GPUI image. Unlike the
//!   arrangement's WGPU path, which is parked until GPU texture compositing
//!   lands, the readback keeps this one *visible* today: a controller strip
//!   is a few hundred pixels tall, so the copy is a fraction of a millisecond
//!   and only happens when the snapshot changes.
//!
//! Neither painter touches project state: the snapshot is built by the lane
//! from the timeline, so both draw exactly the same thing.

use std::hash::{Hash, Hasher};

use gpui::{
    canvas, div, fill, point, px, size, AnyElement, Bounds, IntoElement, ParentElement,
    PathBuilder, PathStyle, Pixels, Rgba, StrokeOptions, Styled,
};

use crate::theme::Colors;

/// One point handle in lane-local logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CcHandle {
    pub x: f32,
    pub y: f32,
    pub selected: bool,
}

/// Everything a painter needs, in lane-local *logical* pixels.
#[derive(Debug, Clone)]
pub(super) struct CcLaneSnapshot {
    pub width: f32,
    pub height: f32,
    /// Device scale, so the GPU painter rasterises at physical resolution.
    pub scale: f32,
    /// Curve y for logical column `i` (`len == ceil(width) + 1`).
    pub samples: Vec<f32>,
    pub handles: Vec<CcHandle>,
    /// y of the controller's default value.
    pub baseline_y: f32,
}

/// Stroke width of the envelope, logical px. Comfortably above 1 px so HiDPI
/// scaling never thins it into shimmer.
const LINE_WIDTH: f32 = 1.8;
const HANDLE_R: f32 = 4.0;
const DASH: f32 = 6.0;
const GAP: f32 = 4.0;

/// Colours shared by both painters so a backend switch changes nothing but
/// the rasteriser.
struct Palette {
    line: Rgba,
    fill_top: Rgba,
    fill_bottom: Rgba,
    baseline: Rgba,
    handle: Rgba,
    ring: Rgba,
}

fn palette() -> Palette {
    Palette {
        line: Colors::accent_primary(),
        fill_top: Colors::with_alpha(Colors::accent_primary(), 0.22),
        fill_bottom: Colors::with_alpha(Colors::accent_primary(), 0.04),
        baseline: Colors::with_alpha(Colors::text_primary(), 0.16),
        handle: Colors::accent_primary(),
        ring: Colors::text_primary(),
    }
}

impl CcLaneSnapshot {
    /// Stable identity of what would be drawn, so the GPU painter re-renders
    /// only when the curve, handles, or geometry actually changed.
    fn content_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.width.to_bits().hash(&mut hasher);
        self.height.to_bits().hash(&mut hasher);
        self.scale.to_bits().hash(&mut hasher);
        self.baseline_y.to_bits().hash(&mut hasher);
        for sample in &self.samples {
            sample.to_bits().hash(&mut hasher);
        }
        for handle in &self.handles {
            handle.x.to_bits().hash(&mut hasher);
            handle.y.to_bits().hash(&mut hasher);
            handle.selected.hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Whether the user's Renderer setting asks for GPU drawing of UI surfaces.
pub(super) fn wgpu_selected() -> bool {
    #[cfg(feature = "gpu-renderer")]
    {
        use crate::components::timeline::render::TimelineRendererBackend;
        TimelineRendererBackend::from_env() == TimelineRendererBackend::Wgpu
    }
    #[cfg(not(feature = "gpu-renderer"))]
    {
        false
    }
}

// ── GPUI batched painter ─────────────────────────────────────────────────────

/// The envelope as one canvas: filled body, stroke, dashed base value, and
/// handle quads. `O(width + points)` per frame, no per-point elements.
pub(super) fn render_gpui(snapshot: &CcLaneSnapshot) -> AnyElement {
    let palette = palette();
    let samples = snapshot.samples.clone();
    let handles = snapshot.handles.clone();
    let view_w = snapshot.width;
    let lane_h = snapshot.height;
    let baseline_y = snapshot.baseline_y;
    canvas(
        |_b, _w, _cx| {},
        move |bounds: Bounds<Pixels>, (), window, _cx| {
            let origin = bounds.origin;

            // Filled body under the curve, closed down to the lane floor.
            if samples.len() >= 2 {
                let mut body = PathBuilder::fill();
                body.move_to(origin + point(px(0.0), px(lane_h)));
                for (col, y) in samples.iter().enumerate() {
                    body.line_to(origin + point(px(col as f32), px(*y)));
                }
                body.line_to(origin + point(px((samples.len() - 1) as f32), px(lane_h)));
                body.close();
                if let Ok(path) = body.build() {
                    // GPUI fills are flat; use the top colour's mid-tone so the
                    // body reads the same weight as the GPU gradient.
                    let mut fill_color = palette.fill_top;
                    fill_color.a = (palette.fill_top.a + palette.fill_bottom.a) * 0.5;
                    window.paint_path(path, fill_color);
                }
            }

            // Dashed base value.
            let mut x = 0.0f32;
            while x < view_w {
                let w = DASH.min(view_w - x);
                let dash = Bounds::new(origin + point(px(x), px(baseline_y)), size(px(w), px(1.0)));
                window.paint_quad(fill(dash, palette.baseline));
                x += DASH + GAP;
            }

            // The envelope: one continuous anti-aliased stroke.
            if samples.len() >= 2 {
                let options = StrokeOptions::default()
                    .with_line_width(LINE_WIDTH)
                    .with_miter_limit(2.0);
                let mut path =
                    PathBuilder::stroke(px(LINE_WIDTH)).with_style(PathStyle::Stroke(options));
                path.move_to(origin + point(px(0.0), px(samples[0])));
                for (col, y) in samples.iter().enumerate().skip(1) {
                    path.line_to(origin + point(px(col as f32), px(*y)));
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, palette.line);
                }
            }

            for handle in &handles {
                // A selected handle is a ring around a filled dot; an unselected
                // one is the plain dot.
                if handle.selected {
                    let ring = Bounds::new(
                        origin + point(px(handle.x - HANDLE_R), px(handle.y - HANDLE_R)),
                        size(px(HANDLE_R * 2.0), px(HANDLE_R * 2.0)),
                    );
                    window.paint_quad(fill(ring, palette.ring));
                }
                let dot_r = if handle.selected {
                    HANDLE_R - 2.0
                } else {
                    HANDLE_R
                };
                let dot = Bounds::new(
                    origin + point(px(handle.x - dot_r), px(handle.y - dot_r)),
                    size(px(dot_r * 2.0), px(dot_r * 2.0)),
                );
                window.paint_quad(fill(dot, palette.handle));
            }
        },
    )
    .absolute()
    .inset_0()
    .into_any_element()
}

// ── WGPU painter (offscreen + readback) ──────────────────────────────────────

/// Rasterise the snapshot on the GPU and present it as a GPUI image. `None`
/// when WGPU is unavailable or the pass failed; the caller then paints with
/// [`render_gpui`], so the lane never goes blank.
#[cfg(feature = "gpu-renderer")]
pub(super) fn render_wgpu(snapshot: &CcLaneSnapshot, cx: &mut gpui::App) -> Option<AnyElement> {
    use gpui::{img, ImageSource, ObjectFit, StyledImage};
    let image = gpu::render(snapshot, cx)?;
    Some(
        div()
            .absolute()
            .inset_0()
            .child(
                img(ImageSource::Render(image))
                    .size_full()
                    .object_fit(ObjectFit::Fill),
            )
            .into_any_element(),
    )
}

#[cfg(not(feature = "gpu-renderer"))]
pub(super) fn render_wgpu(_snapshot: &CcLaneSnapshot, _cx: &mut gpui::App) -> Option<AnyElement> {
    None
}

#[cfg(feature = "gpu-renderer")]
mod gpu {
    use std::cell::RefCell;
    use std::sync::Arc;

    use gpui::RenderImage;
    use image::{Frame, ImageBuffer};
    use smallvec::SmallVec;

    use super::{palette, CcLaneSnapshot, DASH, GAP, HANDLE_R, LINE_WIDTH};

    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
    /// `Globals` in the shader: 24 floats, 96 bytes.
    const GLOBALS_SIZE: u64 = 96;
    /// One handle instance: center (2) + radius (1) + ring flag (1) + fill (4)
    /// + ring colour (4) = 12 floats.
    const HANDLE_INSTANCE_SIZE: u64 = 48;
    /// Above this the strip is wider than any sensible window; leave it to GPUI.
    const MAX_WIDTH: u32 = 8192;

    /// Curve pass: a full-screen triangle pair; the fragment shader evaluates
    /// the sampled envelope per pixel. Handle pass: instanced quads with a
    /// circle SDF. Colours arrive premultiplied; the target starts fully
    /// transparent so the lane's grid shows through under the body.
    const SHADER: &str = r#"
struct Globals {
    viewport: vec2<f32>,
    baseline_y: f32,
    line_half: f32,
    line: vec4<f32>,
    fill_top: vec4<f32>,
    fill_bottom: vec4<f32>,
    baseline: vec4<f32>,
    dash: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var<storage, read> samples: array<f32>;

@vertex
fn vs_fullscreen(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    return vec4<f32>(corners[index], 0.0, 1.0);
}

@fragment
fn fs_curve(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let n = arrayLength(&samples);
    let x = pos.x;
    let i0 = u32(clamp(floor(x), 0.0, f32(n - 1u)));
    let i1 = min(i0 + 1u, n - 1u);
    let t = clamp(x - f32(i0), 0.0, 1.0);
    let y0 = samples[i0];
    let y1 = samples[i1];
    let yc = mix(y0, y1, t);
    let slope = y1 - y0;
    // Perpendicular distance to the local segment, not just vertical.
    let dist = abs(pos.y - yc) / sqrt(1.0 + slope * slope);
    let line_a = 1.0 - smoothstep(g.line_half - 0.6, g.line_half + 0.6, dist);

    // Body under the curve, fading toward the lane floor.
    let below = smoothstep(-0.6, 0.6, pos.y - yc);
    let depth = clamp((pos.y - yc) / max(g.viewport.y - yc, 1.0), 0.0, 1.0);
    let body = mix(g.fill_top, g.fill_bottom, depth) * below;

    // Dashed base value.
    let on_base = 1.0 - smoothstep(0.4, 0.9, abs(pos.y - g.baseline_y));
    let period = g.dash.x + g.dash.y;
    let phase = x - floor(x / period) * period;
    let dash_on = select(0.0, 1.0, phase < g.dash.x);
    let base = g.baseline * on_base * dash_on;

    // Premultiplied "over": body, then base value, then the line on top.
    var out = body;
    out = base + out * (1.0 - base.a);
    let line = g.line * line_a;
    out = line + out * (1.0 - line.a);
    return out;
}

struct HandleInstance {
    @location(0) center_radius_ring: vec4<f32>,
    @location(1) fill: vec4<f32>,
    @location(2) ring: vec4<f32>,
};

struct HandleOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) radius: f32,
    @location(2) ring_flag: f32,
    @location(3) fill: vec4<f32>,
    @location(4) ring: vec4<f32>,
};

@vertex
fn vs_handle(@builtin(vertex_index) index: u32, instance: HandleInstance) -> HandleOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0), vec2<f32>(1.0, 1.0), vec2<f32>(-1.0, 1.0),
    );
    let corner = corners[index];
    let radius = instance.center_radius_ring.z;
    let extent = radius + 1.5;
    let pixel = instance.center_radius_ring.xy + corner * extent;
    let ndc = vec2<f32>(
        pixel.x / max(g.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - pixel.y / max(g.viewport.y, 1.0) * 2.0,
    );
    var out: HandleOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.local = corner * extent;
    out.radius = radius;
    out.ring_flag = instance.center_radius_ring.w;
    out.fill = instance.fill;
    out.ring = instance.ring;
    return out;
}

@fragment
fn fs_handle(in: HandleOut) -> @location(0) vec4<f32> {
    let d = length(in.local);
    let outer = 1.0 - smoothstep(in.radius - 0.7, in.radius + 0.7, d);
    if (in.ring_flag > 0.5) {
        // Ring from `radius - 2` to `radius`, filled dot inside.
        let inner_r = in.radius - 2.0;
        let inner = 1.0 - smoothstep(inner_r - 0.7, inner_r + 0.7, d);
        return in.fill * inner + in.ring * (outer - inner);
    }
    return in.fill * outer;
}
"#;

    struct LaneGpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
        curve_pipeline: wgpu::RenderPipeline,
        handle_pipeline: wgpu::RenderPipeline,
        bind_group_layout: wgpu::BindGroupLayout,
        globals: wgpu::Buffer,
        samples: wgpu::Buffer,
        samples_capacity: u64,
        handles: wgpu::Buffer,
        handles_capacity: u64,
        target: Option<(wgpu::Texture, wgpu::TextureView, u32, u32)>,
        readback: Option<(wgpu::Buffer, u64)>,
        last_hash: u64,
        last_image: Option<Arc<RenderImage>>,
    }

    thread_local! {
        /// `None` = not tried yet; `Some(None)` = unavailable on this machine.
        static LANE_GPU: RefCell<Option<Option<LaneGpu>>> = const { RefCell::new(None) };
    }

    fn gpu_debug() -> bool {
        std::env::var_os("FUTUREBOARD_GPU_RENDERER_DEBUG").is_some()
    }

    pub(super) fn render(
        snapshot: &CcLaneSnapshot,
        cx: &mut gpui::App,
    ) -> Option<Arc<RenderImage>> {
        LANE_GPU.with(|cell| {
            let mut slot = cell.borrow_mut();
            let gpu = slot.get_or_insert_with(|| match LaneGpu::new() {
                Ok(gpu) => Some(gpu),
                Err(error) => {
                    eprintln!(
                        "[gpu-renderer] controller lane WGPU unavailable: {error}; using GPUI paint"
                    );
                    None
                }
            });
            let gpu = gpu.as_mut()?;
            let hash = snapshot.content_hash();
            if gpu.last_hash == hash {
                if let Some(image) = gpu.last_image.clone() {
                    return Some(image);
                }
            }
            match gpu.render(snapshot) {
                Ok(image) => {
                    // The previous frame's texture must be released from GPUI's
                    // atlas, or every repaint would leak one.
                    if let Some(previous) = gpu.last_image.replace(image.clone()) {
                        cx.drop_image(previous, None);
                    }
                    gpu.last_hash = hash;
                    Some(image)
                }
                Err(error) => {
                    if gpu_debug() {
                        eprintln!("[gpu-renderer] controller lane pass failed: {error}");
                    }
                    None
                }
            }
        })
    }

    fn premultiplied(color: gpui::Rgba) -> [f32; 4] {
        [
            color.r * color.a,
            color.g * color.a,
            color.b * color.a,
            color.a,
        ]
    }

    impl LaneGpu {
        fn new() -> Result<Self, String> {
            let instance = wgpu::Instance::default();
            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::LowPower,
                    compatible_surface: None,
                    force_fallback_adapter: false,
                }))
                .map_err(|_| "no WGPU adapter".to_string())?;
            let limits = wgpu::Limits::downlevel_defaults();
            if limits.max_storage_buffers_per_shader_stage == 0 {
                return Err("adapter has no fragment storage buffers".to_string());
            }
            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("futureboard-controller-lane"),
                    required_features: wgpu::Features::empty(),
                    required_limits: limits,
                    memory_hints: wgpu::MemoryHints::Performance,
                    trace: wgpu::Trace::Off,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                }))
                .map_err(|error| format!("device creation failed: {error}"))?;

            // Pipeline creation is validated inside an error scope so an
            // adapter that rejects the shader degrades to GPUI paint instead
            // of panicking the UI thread.
            let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("controller-lane-shader"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
            let bind_group_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("controller-lane-globals-layout"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Storage { read_only: true },
                                has_dynamic_offset: false,
                                min_binding_size: None,
                            },
                            count: None,
                        },
                    ],
                });
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("controller-lane-layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
            let target = wgpu::ColorTargetState {
                format: FORMAT,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            };
            let curve_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("controller-lane-curve"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_curve"),
                    compilation_options: Default::default(),
                    targets: &[Some(target.clone())],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            let handle_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("controller-lane-handles"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_handle"),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: HANDLE_INSTANCE_SIZE,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 16,
                                shader_location: 1,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x4,
                                offset: 32,
                                shader_location: 2,
                            },
                        ],
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_handle"),
                    compilation_options: Default::default(),
                    targets: &[Some(target)],
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
                label: Some("controller-lane-globals"),
                size: GLOBALS_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let samples_capacity = 4096;
            let samples = Self::samples_buffer(&device, samples_capacity);
            let handles_capacity = 256;
            let handles = Self::handles_buffer(&device, handles_capacity);
            Ok(Self {
                device,
                queue,
                curve_pipeline,
                handle_pipeline,
                bind_group_layout,
                globals,
                samples,
                samples_capacity,
                handles,
                handles_capacity,
                target: None,
                readback: None,
                last_hash: 0,
                last_image: None,
            })
        }

        fn samples_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("controller-lane-samples"),
                size: capacity * 4,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        }

        fn handles_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("controller-lane-handles"),
                size: capacity * HANDLE_INSTANCE_SIZE,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        }

        fn ensure_target(&mut self, width: u32, height: u32) {
            if self
                .target
                .as_ref()
                .is_some_and(|(_, _, w, h)| *w == width && *h == height)
            {
                return;
            }
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("controller-lane-offscreen"),
                size: wgpu::Extent3d {
                    width,
                    height,
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
            self.target = Some((texture, view, width, height));
        }

        fn ensure_readback(&mut self, bytes: u64) -> &wgpu::Buffer {
            if self.readback.as_ref().is_none_or(|(_, size)| *size < bytes) {
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("controller-lane-readback"),
                    size: bytes,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                self.readback = Some((buffer, bytes));
            }
            &self.readback.as_ref().expect("readback").0
        }

        fn render(&mut self, snapshot: &CcLaneSnapshot) -> Result<Arc<RenderImage>, String> {
            let scale = snapshot.scale.max(0.5);
            let width = (snapshot.width * scale).round().max(1.0) as u32;
            let height = (snapshot.height * scale).round().max(1.0) as u32;
            if width > MAX_WIDTH || snapshot.samples.len() < 2 {
                return Err("lane too wide or empty for the GPU pass".to_string());
            }

            // Resample the logical-column curve at physical columns.
            let physical: Vec<f32> = (0..=width)
                .map(|col| {
                    let logical = col as f32 / scale;
                    let i0 = (logical.floor() as usize).min(snapshot.samples.len() - 1);
                    let i1 = (i0 + 1).min(snapshot.samples.len() - 1);
                    let t = (logical - i0 as f32).clamp(0.0, 1.0);
                    (snapshot.samples[i0] + (snapshot.samples[i1] - snapshot.samples[i0]) * t)
                        * scale
                })
                .collect();
            if physical.len() as u64 > self.samples_capacity {
                self.samples_capacity = (physical.len() as u64).next_power_of_two();
                self.samples = Self::samples_buffer(&self.device, self.samples_capacity);
            }
            let sample_bytes: Vec<u8> = physical.iter().flat_map(|v| v.to_ne_bytes()).collect();
            // The shader reads `arrayLength`, so the bound range must be exactly
            // this frame's samples, not the whole capacity.
            self.queue.write_buffer(&self.samples, 0, &sample_bytes);

            let palette = palette();
            let mut instances: Vec<u8> = Vec::with_capacity(snapshot.handles.len() * 48);
            for handle in &snapshot.handles {
                let x = handle.x * scale;
                let y = handle.y * scale;
                if x < -HANDLE_R * scale || x > width as f32 + HANDLE_R * scale {
                    continue;
                }
                let values: [f32; 12] = [
                    x,
                    y,
                    HANDLE_R * scale,
                    if handle.selected { 1.0 } else { 0.0 },
                    premultiplied(palette.handle)[0],
                    premultiplied(palette.handle)[1],
                    premultiplied(palette.handle)[2],
                    premultiplied(palette.handle)[3],
                    premultiplied(palette.ring)[0],
                    premultiplied(palette.ring)[1],
                    premultiplied(palette.ring)[2],
                    premultiplied(palette.ring)[3],
                ];
                instances.extend(values.iter().flat_map(|v| v.to_ne_bytes()));
            }
            let handle_count = (instances.len() as u64) / HANDLE_INSTANCE_SIZE;
            if handle_count > self.handles_capacity {
                self.handles_capacity = handle_count.next_power_of_two();
                self.handles = Self::handles_buffer(&self.device, self.handles_capacity);
            }
            if handle_count > 0 {
                self.queue.write_buffer(&self.handles, 0, &instances);
            }

            // Mirrors the shader's `Globals` layout: 24 floats.
            let mut globals = [0f32; 24];
            globals[0] = width as f32;
            globals[1] = height as f32;
            globals[2] = snapshot.baseline_y * scale;
            globals[3] = LINE_WIDTH * scale * 0.5;
            globals[4..8].copy_from_slice(&premultiplied(palette.line));
            globals[8..12].copy_from_slice(&premultiplied(palette.fill_top));
            globals[12..16].copy_from_slice(&premultiplied(palette.fill_bottom));
            globals[16..20].copy_from_slice(&premultiplied(palette.baseline));
            globals[20] = DASH * scale;
            globals[21] = GAP * scale;
            let globals_bytes: Vec<u8> = globals.iter().flat_map(|v| v.to_ne_bytes()).collect();
            debug_assert_eq!(globals_bytes.len() as u64, GLOBALS_SIZE);
            self.queue.write_buffer(&self.globals, 0, &globals_bytes);

            self.ensure_target(width, height);
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("controller-lane-bind-group"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.globals.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &self.samples,
                            offset: 0,
                            size: wgpu::BufferSize::new(sample_bytes.len() as u64),
                        }),
                    },
                ],
            });

            let unpadded = width * 4;
            let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
            let padded = unpadded.div_ceil(align) * align;
            let readback_bytes = (padded * height) as u64;
            let (texture, view, _, _) = self.target.as_ref().expect("target");
            let texture = texture.clone();
            let view = view.clone();
            let readback = self.ensure_readback(readback_bytes).clone();

            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("controller-lane"),
                });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("controller-lane-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
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
                pass.set_bind_group(0, &bind_group, &[]);
                pass.set_pipeline(&self.curve_pipeline);
                pass.draw(0..6, 0..1);
                if handle_count > 0 {
                    pass.set_pipeline(&self.handle_pipeline);
                    pass.set_vertex_buffer(
                        0,
                        self.handles.slice(..handle_count * HANDLE_INSTANCE_SIZE),
                    );
                    pass.draw(0..6, 0..handle_count as u32);
                }
            }
            encoder.copy_texture_to_buffer(
                texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
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
            let mut bgra = Vec::with_capacity((unpadded * height) as usize);
            for row in 0..height {
                let start = (row * padded) as usize;
                for px in mapped[start..start + unpadded as usize].chunks_exact(4) {
                    bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            drop(mapped);
            readback.unmap();

            let buffer = ImageBuffer::from_raw(width, height, bgra)
                .ok_or_else(|| "readback size mismatch".to_string())?;
            Ok(Arc::new(RenderImage::new(SmallVec::from_elem(
                Frame::new(buffer),
                1,
            ))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(samples: Vec<f32>) -> CcLaneSnapshot {
        CcLaneSnapshot {
            width: (samples.len() - 1) as f32,
            height: 140.0,
            scale: 1.0,
            samples,
            handles: vec![CcHandle {
                x: 10.0,
                y: 20.0,
                selected: false,
            }],
            baseline_y: 135.0,
        }
    }

    #[test]
    fn content_hash_tracks_curve_and_handles_only() {
        let a = snapshot(vec![10.0, 20.0, 30.0]);
        let same = snapshot(vec![10.0, 20.0, 30.0]);
        assert_eq!(a.content_hash(), same.content_hash());

        let moved_curve = snapshot(vec![10.0, 21.0, 30.0]);
        assert_ne!(a.content_hash(), moved_curve.content_hash());

        let mut selected = snapshot(vec![10.0, 20.0, 30.0]);
        selected.handles[0].selected = true;
        assert_ne!(a.content_hash(), selected.content_hash());

        let mut rescaled = snapshot(vec![10.0, 20.0, 30.0]);
        rescaled.scale = 2.0;
        assert_ne!(a.content_hash(), rescaled.content_hash());
    }
}
