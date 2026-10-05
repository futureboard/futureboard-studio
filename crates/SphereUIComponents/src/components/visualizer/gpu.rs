//! wgpu renderer for the visualizers.
//!
//! The scene (triangles, the spectrogram texture) is drawn by wgpu into an
//! off-screen colour target, and GPUI composites that target inside the
//! visualizer window like any other image. Two ways the target reaches GPUI:
//!
//! * **Shared (Windows)** — wgpu runs on D3D12 on the same adapter as GPUI's
//!   D3D11 renderer and draws into a texture created with a shared NT handle.
//!   GPUI's atlas opens that handle and copies GPU-to-GPU: no pixel crosses the
//!   CPU. The adapter is matched by LUID, which GPUI reports.
//! * **Readback (everywhere else, or if the shared path fails)** — the target
//!   is copied to a mapped buffer and handed to GPUI as a `RenderImage`.
//!
//! Everything runs on the UI thread: rendering finishes (the queue is waited
//! on) before the frame is handed over, so GPUI never reads a half-drawn
//! target.

use std::cell::RefCell;
use std::rc::Rc;

use super::analysis::DISPLAY_BINS;
use super::scene::{Scene, VERTEX_FLOATS, Vertex};

/// BGRA, not sRGB: theme colours land in the target with the numeric values
/// GPUI paints them with, and GPUI's atlas imports exactly this format.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
const VERTEX_STRIDE: u64 = (VERTEX_FLOATS * 4) as u64;
/// Spectrogram history columns: at the window's ~60 Hz, about eight seconds.
pub const SPECTROGRAM_COLUMNS: u32 = 480;

const SHADER: &str = r#"
struct Globals {
    size: vec2<f32>,
    _pad: vec2<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> Out {
    var out: Out;
    out.position = vec4<f32>(
        position.x / globals.size.x * 2.0 - 1.0,
        1.0 - position.y / globals.size.y * 2.0,
        0.0,
        1.0,
    );
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: Out) -> @location(0) vec4<f32> {
    return in.color;
}

struct Spectro {
    rect: vec4<f32>,
    // x: horizontal scroll so the newest column lands on the right edge.
    scroll: vec4<f32>,
};
@group(1) @binding(0) var<uniform> spectro: Spectro;
@group(1) @binding(1) var history: texture_2d<f32>;
@group(1) @binding(2) var history_sampler: sampler;

struct SpectroOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn spectro_vs(@builtin(vertex_index) index: u32) -> SpectroOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let corner = corners[index];
    let pixel = spectro.rect.xy + corner * spectro.rect.zw;
    var out: SpectroOut;
    out.position = vec4<f32>(
        pixel.x / globals.size.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.size.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = corner;
    return out;
}

@fragment
fn spectro_fs(in: SpectroOut) -> @location(0) vec4<f32> {
    // Row 0 of the history is the lowest frequency, drawn at the bottom.
    let uv = vec2<f32>(fract(in.uv.x + spectro.scroll.x), 1.0 - in.uv.y);
    return textureSample(history, history_sampler, uv);
}
"#;

/// The device, queue and pipelines, shared by every visualizer window.
pub struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    alpha: wgpu::RenderPipeline,
    glow: wgpu::RenderPipeline,
    spectro: wgpu::RenderPipeline,
    globals_layout: wgpu::BindGroupLayout,
    spectro_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Whether shared textures can be created (D3D12 on GPUI's adapter).
    shared: bool,
    pub adapter_name: String,
}

thread_local! {
    /// Lives as long as the process and is never dropped: GPUI's Windows
    /// platform ends in `ExitProcess`, which runs thread-local destructors, and
    /// dropping a wgpu queue there touches wgpu's own thread-locals after they
    /// are gone — the process aborts on its way out. The OS reclaims it.
    static CONTEXT: std::mem::ManuallyDrop<RefCell<Option<Result<Rc<GpuContext>, String>>>> =
        const { std::mem::ManuallyDrop::new(RefCell::new(None)) };
}

/// The shared context, created on first use. `adapter_luid` is GPUI's D3D11
/// adapter; the first window to ask decides it for the process, which is
/// correct because every GPUI window shares one device.
pub fn context(adapter_luid: Option<u64>) -> Result<Rc<GpuContext>, String> {
    CONTEXT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(GpuContext::new(adapter_luid).map(Rc::new));
        }
        slot.as_ref().expect("just set").clone()
    })
}

impl GpuContext {
    fn new(adapter_luid: Option<u64>) -> Result<Self, String> {
        let (adapter, shared) = pick_adapter(adapter_luid)?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("futureboard-visualizer"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }))
        .map_err(|error| format!("visualizer device: {error}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("visualizer-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("visualizer-globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let spectro_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("visualizer-spectrogram"),
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
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let geometry_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("visualizer-geometry"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });
        let spectro_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("visualizer-spectrogram"),
                bind_group_layouts: &[Some(&globals_layout), Some(&spectro_layout)],
                immediate_size: 0,
            });

        // Colour blends as usual; the target's alpha is left at 1 so GPUI
        // composites an opaque image.
        let keep_alpha = wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::Zero,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        };
        let alpha_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: keep_alpha,
        };
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: keep_alpha,
        };
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: VERTEX_STRIDE,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 8,
                    shader_location: 1,
                },
            ],
        };
        let geometry_pipeline = |label: &str, blend: wgpu::BlendState| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&geometry_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: std::slice::from_ref(&vertex_layout),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let alpha = geometry_pipeline("visualizer-alpha", alpha_blend);
        let glow = geometry_pipeline("visualizer-glow", additive);
        let spectro = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("visualizer-spectrogram"),
            layout: Some(&spectro_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("spectro_vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("spectro_fs"),
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
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("visualizer-spectrogram"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            device,
            queue,
            alpha,
            glow,
            spectro,
            globals_layout,
            spectro_layout,
            sampler,
            shared,
            adapter_name: info.name,
        })
    }

    pub fn shares_textures(&self) -> bool {
        self.shared
    }
}

/// The adapter to render on, and whether shared textures work on it.
fn pick_adapter(adapter_luid: Option<u64>) -> Result<(wgpu::Adapter, bool), String> {
    // D3D12 only on Windows: it is what the shared-texture path needs, and
    // its WARP adapter covers the fallback. Letting the GL backend start as
    // well left a GL context in the thread-local `CONTEXT`, whose teardown
    // at process exit panics inside wgpu-hal's WGL code and aborts.
    #[cfg(target_os = "windows")]
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    #[cfg(not(target_os = "windows"))]
    let instance = wgpu::Instance::default();
    #[cfg(target_os = "windows")]
    if let Some(luid) = adapter_luid {
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
        for adapter in adapters {
            if dx12_adapter_luid(&adapter) == Some(luid) {
                return Ok((adapter, true));
            }
        }
    }
    let _ = adapter_luid;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .or_else(|_| {
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: true,
        }))
    })
    .map_err(|_| "no GPU adapter for the visualizer".to_string())?;
    Ok((adapter, false))
}

#[cfg(target_os = "windows")]
fn dx12_adapter_luid(adapter: &wgpu::Adapter) -> Option<u64> {
    // SAFETY: the hal adapter is only read (its DXGI description) while the
    // wgpu adapter it came from is alive.
    let hal = unsafe { adapter.as_hal::<wgpu::hal::api::Dx12>() }?;
    let desc = unsafe { hal.raw_adapter().GetDesc1() }.ok()?;
    Some(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64)
}

/// What a finished frame hands to GPUI.
pub enum Presented {
    /// A shared NT handle GPUI's atlas can open, and the texture's size.
    Shared {
        handle: usize,
        width: u32,
        height: u32,
    },
    /// Straight BGRA pixels, row-major, `width * 4` bytes a row.
    Pixels {
        bgra: Vec<u8>,
        width: u32,
        height: u32,
    },
}

/// One window's render target and per-window GPU state.
pub struct Surface {
    context: Rc<GpuContext>,
    target: Target,
    globals: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    vertices: wgpu::Buffer,
    vertex_capacity: u64,
    vertex_bytes: Vec<u8>,
    spectrogram: Option<Spectrogram>,
    /// Whether the target may be a shared texture (see [`Presented::Shared`]).
    allow_shared: bool,
    /// The target has not been drawn yet: its content (alpha included) is
    /// undefined, so a persistent frame cannot fade it and clears it instead.
    fresh: bool,
}

struct Target {
    width: u32,
    height: u32,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    #[cfg(target_os = "windows")]
    shared: Option<shared::SharedHandle>,
    readback: Option<wgpu::Buffer>,
}

struct Spectrogram {
    texture: wgpu::Texture,
    uniforms: wgpu::Buffer,
    bind: wgpu::BindGroup,
    /// The column the next write goes to.
    write: u32,
}

impl Surface {
    pub fn new(
        context: Rc<GpuContext>,
        width: u32,
        height: u32,
        allow_shared: bool,
    ) -> Result<Self, String> {
        let device = &context.device;
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("visualizer-globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("visualizer-globals"),
            layout: &context.globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let vertex_capacity = 16_384;
        let vertices = create_vertex_buffer(device, vertex_capacity);
        let target = create_target(&context, width.max(1), height.max(1), allow_shared)?;
        Ok(Self {
            context,
            target,
            globals,
            globals_bind,
            vertices,
            vertex_capacity,
            vertex_bytes: Vec::new(),
            spectrogram: None,
            allow_shared,
            fresh: true,
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.target.width, self.target.height)
    }

    /// Recreate the target for a new size. The previous frame's content is
    /// dropped, which only a persistent view notices, for one frame.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == self.size() {
            return Ok(());
        }
        self.target = create_target(&self.context, width, height, self.allow_shared)?;
        self.fresh = true;
        Ok(())
    }

    /// Append one spectrogram column (`DISPLAY_BINS` RGBA texels, lowest
    /// frequency first).
    pub fn push_spectrogram_column(&mut self, texels: &[[u8; 4]; DISPLAY_BINS]) {
        let context = &self.context;
        let spectrogram = self
            .spectrogram
            .get_or_insert_with(|| create_spectrogram(context));
        let bytes: Vec<u8> = texels.iter().flatten().copied().collect();
        context.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &spectrogram.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: spectrogram.write,
                    y: 0,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(DISPLAY_BINS as u32),
            },
            wgpu::Extent3d {
                width: 1,
                height: DISPLAY_BINS as u32,
                depth_or_array_layers: 1,
            },
        );
        spectrogram.write = (spectrogram.write + 1) % SPECTROGRAM_COLUMNS;
    }

    /// Draw `scene` and hand the result over.
    pub fn render(&mut self, scene: &Scene) -> Result<Presented, String> {
        let context = self.context.clone();
        let (width, height) = self.size();
        let mut globals = [0u8; 16];
        globals[0..4].copy_from_slice(&(width as f32).to_ne_bytes());
        globals[4..8].copy_from_slice(&(height as f32).to_ne_bytes());
        context.queue.write_buffer(&self.globals, 0, &globals);

        // One buffer: [fade quad][alpha triangles][glow triangles].
        let fade: Vec<Vertex> = scene
            .fade
            .map(|color| {
                let (w, h) = (width as f32, height as f32);
                [[0.0, 0.0], [w, 0.0], [w, h], [0.0, 0.0], [w, h], [0.0, h]]
                    .map(|position| Vertex { position, color })
                    .to_vec()
            })
            .unwrap_or_default();
        self.vertex_bytes.clear();
        for vertex in fade.iter().chain(&scene.triangles).chain(&scene.glow) {
            for value in vertex.position.iter().chain(&vertex.color) {
                self.vertex_bytes.extend_from_slice(&value.to_ne_bytes());
            }
        }
        let vertex_count = (self.vertex_bytes.len() as u64) / VERTEX_STRIDE;
        if vertex_count > self.vertex_capacity {
            self.vertex_capacity = vertex_count.next_power_of_two();
            self.vertices = create_vertex_buffer(&context.device, self.vertex_capacity);
        }
        if !self.vertex_bytes.is_empty() {
            context
                .queue
                .write_buffer(&self.vertices, 0, &self.vertex_bytes);
        }
        if let (Some(rect), Some(spectrogram)) = (scene.spectrogram, self.spectrogram.as_ref()) {
            let mut uniforms = [0u8; 32];
            for (i, value) in rect.iter().enumerate() {
                uniforms[i * 4..i * 4 + 4].copy_from_slice(&value.to_ne_bytes());
            }
            let scroll = spectrogram.write as f32 / SPECTROGRAM_COLUMNS as f32;
            uniforms[16..20].copy_from_slice(&scroll.to_ne_bytes());
            context
                .queue
                .write_buffer(&spectrogram.uniforms, 0, &uniforms);
        }

        let mut encoder = context
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("visualizer-frame"),
            });
        {
            // The pipelines keep the target's alpha, and GPUI composites the
            // target with it: a fading view's first frame has to lay down an
            // opaque background, or the whole picture stays transparent.
            let clear = match (scene.clear, scene.fade) {
                (Some(color), _) => Some(color),
                (None, Some([r, g, b, _])) if self.fresh => Some([r, g, b, 1.0]),
                _ => None,
            };
            let load = match clear {
                Some([r, g, b, a]) => wgpu::LoadOp::Clear(wgpu::Color {
                    r: r as f64,
                    g: g as f64,
                    b: b as f64,
                    a: a as f64,
                }),
                None => wgpu::LoadOp::Load,
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("visualizer-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.globals_bind, &[]);
            let fade_count = fade.len() as u32;
            let alpha_count = scene.triangles.len() as u32;
            let glow_count = scene.glow.len() as u32;
            if vertex_count > 0 {
                pass.set_vertex_buffer(0, self.vertices.slice(..vertex_count * VERTEX_STRIDE));
            }
            if fade_count > 0 {
                pass.set_pipeline(&context.alpha);
                pass.draw(0..fade_count, 0..1);
            }
            if let (Some(_), Some(spectrogram)) = (scene.spectrogram, self.spectrogram.as_ref()) {
                pass.set_pipeline(&context.spectro);
                pass.set_bind_group(1, &spectrogram.bind, &[]);
                pass.draw(0..6, 0..1);
            }
            if alpha_count > 0 {
                pass.set_pipeline(&context.alpha);
                pass.draw(fade_count..fade_count + alpha_count, 0..1);
            }
            if glow_count > 0 {
                pass.set_pipeline(&context.glow);
                let start = fade_count + alpha_count;
                pass.draw(start..start + glow_count, 0..1);
            }
        }

        let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        if let Some(readback) = self.target.readback.as_ref() {
            encoder.copy_texture_to_buffer(
                self.target.texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_row),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        context.queue.submit(Some(encoder.finish()));
        self.fresh = false;

        #[cfg(target_os = "windows")]
        if let Some(shared) = self.target.shared.as_ref() {
            // GPUI copies from the texture as soon as it has the handle, on
            // another API: the frame has to be complete first.
            context
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .map_err(|error| format!("visualizer frame wait: {error}"))?;
            return Ok(Presented::Shared {
                handle: shared.raw(),
                width,
                height,
            });
        }

        let readback = self
            .target
            .readback
            .as_ref()
            .ok_or("visualizer target has neither a shared handle nor a readback buffer")?;
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        context
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .map_err(|error| format!("visualizer readback wait: {error}"))?;
        let mapped = readback.slice(..).get_mapped_range();
        let row = (width * 4) as usize;
        let mut bgra = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = y * padded_row as usize;
            bgra.extend_from_slice(&mapped[start..start + row]);
        }
        drop(mapped);
        readback.unmap();
        Ok(Presented::Pixels {
            bgra,
            width,
            height,
        })
    }
}

fn create_vertex_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("visualizer-vertices"),
        size: capacity * VERTEX_STRIDE,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_target(
    context: &GpuContext,
    width: u32,
    height: u32,
    allow_shared: bool,
) -> Result<Target, String> {
    let descriptor = wgpu::TextureDescriptor {
        label: Some("visualizer-target"),
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
    };
    #[cfg(target_os = "windows")]
    if allow_shared && context.shared {
        match shared::create(&context.device, &descriptor) {
            Ok((texture, handle)) => {
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                return Ok(Target {
                    width,
                    height,
                    texture,
                    view,
                    shared: Some(handle),
                    readback: None,
                });
            }
            Err(error) => {
                eprintln!("[visualizer] shared texture unavailable, reading back instead: {error}");
            }
        }
    }
    let _ = allow_shared;
    let texture = context.device.create_texture(&descriptor);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let padded_row = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = context.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("visualizer-readback"),
        size: padded_row as u64 * height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    Ok(Target {
        width,
        height,
        texture,
        view,
        #[cfg(target_os = "windows")]
        shared: None,
        readback: Some(readback),
    })
}

fn create_spectrogram(context: &GpuContext) -> Spectrogram {
    let device = &context.device;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("visualizer-spectrogram-history"),
        size: wgpu::Extent3d {
            width: SPECTROGRAM_COLUMNS,
            height: DISPLAY_BINS as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("visualizer-spectrogram-uniforms"),
        size: 32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("visualizer-spectrogram"),
        layout: &context.spectro_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&context.sampler),
            },
        ],
    });
    Spectrogram {
        texture,
        uniforms,
        bind,
        write: 0,
    }
}

/// D3D12 textures with a shared NT handle, wrapped as wgpu textures.
#[cfg(target_os = "windows")]
mod shared {
    use windows_d3d12::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
    use windows_d3d12::Win32::Graphics::Direct3D12::{
        D3D12_HEAP_FLAG_SHARED, D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_DESC, D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
        D3D12_RESOURCE_STATE_COMMON, D3D12_TEXTURE_LAYOUT_UNKNOWN, ID3D12Resource,
    };
    use windows_d3d12::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };

    /// An NT handle to a shared texture, closed on drop.
    pub struct SharedHandle(HANDLE);

    impl SharedHandle {
        pub fn raw(&self) -> usize {
            self.0.0 as usize
        }
    }

    impl Drop for SharedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle came from `CreateSharedHandle` and is closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Create a render target D3D11 can open: a committed D3D12 resource on a
    /// shared heap with simultaneous access (so it decays to the common state
    /// between command lists and GPUI's copy needs no barrier from us).
    pub fn create(
        device: &wgpu::Device,
        descriptor: &wgpu::TextureDescriptor<'_>,
    ) -> Result<(wgpu::Texture, SharedHandle), String> {
        // SAFETY: the hal device is only used to create one resource and a
        // handle to it, while the wgpu device it belongs to is alive.
        let hal_device = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }
            .ok_or("the visualizer device is not D3D12")?;
        let raw = hal_device.raw_device();
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: descriptor.size.width as u64,
            Height: descriptor.size.height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
        };
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: plain D3D12 calls on a live device with valid descriptors.
        unsafe {
            raw.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_SHARED,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
        }
        .map_err(|error| format!("CreateCommittedResource: {error}"))?;
        let resource = resource.ok_or("CreateCommittedResource returned no resource")?;
        let handle = unsafe { raw.CreateSharedHandle(&resource, None, GENERIC_ALL.0, None) }
            .map_err(|error| format!("CreateSharedHandle: {error}"))?;
        let handle = SharedHandle(handle);
        // SAFETY: the resource matches `descriptor` (format, size, one mip,
        // one sample) and is handed to wgpu, which owns it from here.
        let hal_texture = unsafe {
            wgpu::hal::dx12::Device::texture_from_raw(
                resource,
                descriptor.format,
                descriptor.dimension,
                descriptor.size,
                1,
                1,
            )
        };
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Dx12>(hal_texture, descriptor)
        };
        Ok((texture, handle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A GPU is not guaranteed on every machine that runs the tests; without
    /// one these tests have nothing to check and pass vacuously.
    fn context() -> Option<Rc<GpuContext>> {
        match GpuContext::new(None) {
            Ok(context) => Some(Rc::new(context)),
            Err(error) => {
                eprintln!("[visualizer-test] no GPU: {error}");
                None
            }
        }
    }

    fn quad(x0: f32, y0: f32, x1: f32, y1: f32, color: [f32; 4]) -> Vec<Vertex> {
        [[x0, y0], [x1, y0], [x1, y1], [x0, y0], [x1, y1], [x0, y1]]
            .map(|position| Vertex { position, color })
            .to_vec()
    }

    fn pixel(bgra: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * width + x) * 4) as usize;
        [bgra[i], bgra[i + 1], bgra[i + 2], bgra[i + 3]]
    }

    #[test]
    fn a_frame_draws_its_triangles_over_the_clear_colour() {
        let Some(context) = context() else { return };
        let mut surface = Surface::new(context, 64, 32, false).expect("surface");
        let scene = Scene {
            clear: Some([0.0, 0.0, 0.0, 1.0]),
            triangles: quad(0.0, 0.0, 32.0, 32.0, [1.0, 0.0, 0.0, 1.0]),
            glow: quad(40.0, 0.0, 64.0, 32.0, [0.0, 0.0, 1.0, 0.5]),
            ..Scene::default()
        };
        let Presented::Pixels {
            bgra,
            width,
            height,
        } = surface.render(&scene).expect("frame")
        else {
            panic!("a readback surface hands back pixels");
        };
        assert_eq!((width, height), (64, 32));
        assert_eq!(pixel(&bgra, width, 8, 8), [0, 0, 255, 255], "red, as BGRA");
        assert_eq!(
            pixel(&bgra, width, 36, 8),
            [0, 0, 0, 255],
            "the clear colour between"
        );
        // Additive at half alpha over black: half blue, alpha kept opaque.
        let glow = pixel(&bgra, width, 50, 8);
        assert!(
            (glow[0] as i32 - 128).abs() <= 2 && glow[3] == 255,
            "{glow:?}"
        );
    }

    #[test]
    fn a_persistent_frame_fades_the_last_one_instead_of_clearing_it() {
        let Some(context) = context() else { return };
        let mut surface = Surface::new(context, 16, 16, false).expect("surface");
        let lit = Scene {
            clear: Some([0.0, 0.0, 0.0, 1.0]),
            triangles: quad(0.0, 0.0, 16.0, 16.0, [1.0, 1.0, 1.0, 1.0]),
            ..Scene::default()
        };
        surface.render(&lit).expect("first frame");
        let faded = Scene {
            fade: Some([0.0, 0.0, 0.0, 0.5]),
            ..Scene::default()
        };
        let Presented::Pixels { bgra, width, .. } = surface.render(&faded).expect("second frame")
        else {
            panic!("readback");
        };
        let value = pixel(&bgra, width, 8, 8)[1] as i32;
        assert!(
            (value - 128).abs() <= 2,
            "half of white survives one fade: {value}"
        );
    }

    #[test]
    fn a_fading_view_starts_from_an_opaque_background() {
        let Some(context) = context() else { return };
        let mut surface = Surface::new(context, 16, 16, false).expect("surface");
        let first = Scene {
            fade: Some([0.25, 0.25, 0.25, 0.3]),
            ..Scene::default()
        };
        let Presented::Pixels { bgra, width, .. } = surface.render(&first).expect("frame") else {
            panic!("readback");
        };
        assert_eq!(pixel(&bgra, width, 8, 8), [64, 64, 64, 255]);
        // A resized target is new again.
        surface.resize(8, 8).expect("resize");
        let Presented::Pixels { bgra, width, .. } = surface.render(&first).expect("frame") else {
            panic!("readback");
        };
        assert_eq!(pixel(&bgra, width, 4, 4)[3], 255);
    }

    #[test]
    fn spectrogram_columns_scroll_into_the_frame() {
        let Some(context) = context() else { return };
        // One history column per pixel.
        let mut surface = Surface::new(context, SPECTROGRAM_COLUMNS, 32, false).expect("surface");
        surface.push_spectrogram_column(&[[255, 255, 255, 255]; DISPLAY_BINS]);
        let scene = Scene {
            clear: Some([0.0, 0.0, 0.0, 1.0]),
            spectrogram: Some([0.0, 0.0, SPECTROGRAM_COLUMNS as f32, 32.0]),
            ..Scene::default()
        };
        let Presented::Pixels { bgra, width, .. } = surface.render(&scene).expect("frame") else {
            panic!("readback");
        };
        // The one written column is the newest, at the right edge.
        let newest = pixel(&bgra, width, width - 1, 16);
        assert!(newest[0] > 200, "{newest:?}");
        assert!(pixel(&bgra, width, width / 2, 16)[0] < 20);
    }

    /// The Windows path end to end on the D3D12 side: a shared, render-target
    /// texture with a live NT handle that wgpu can draw into.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_d3d12_device_creates_a_shared_render_target() {
        let instance = wgpu::Instance::default();
        let Some(adapter) = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::DX12))
            .into_iter()
            .next()
        else {
            return;
        };
        assert!(dx12_adapter_luid(&adapter).is_some());
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("device");
        let descriptor = wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 64,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };
        let (texture, handle) = shared::create(&device, &descriptor).expect("shared texture");
        assert_ne!(handle.raw(), 0);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::RED),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        queue.submit(Some(encoder.finish()));
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("wait");

        // The other side, as GPUI's atlas does it: a D3D11 device on the same
        // adapter opens the NT handle and sees the finished frame.
        let luid = dx12_adapter_luid(&adapter).expect("luid");
        let red = d3d11::read_first_pixel(luid, handle.raw()).expect("D3D11 opens the handle");
        assert_eq!(red, [0, 0, 255, 255], "red as BGRA, visible to D3D11");
    }

    /// Just enough D3D11 to stand in for GPUI's atlas in a test.
    #[cfg(target_os = "windows")]
    mod d3d11 {
        use windows::Win32::Foundation::{HANDLE, HMODULE};
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
        use windows::core::Interface;

        pub fn read_first_pixel(luid: u64, handle: usize) -> windows::core::Result<[u8; 4]> {
            unsafe {
                let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
                let mut index = 0;
                let adapter: IDXGIAdapter1 = loop {
                    let adapter = factory.EnumAdapters1(index)?;
                    let desc = adapter.GetDesc1()?;
                    let id = ((desc.AdapterLuid.HighPart as u32 as u64) << 32)
                        | desc.AdapterLuid.LowPart as u64;
                    if id == luid {
                        break adapter;
                    }
                    index += 1;
                };
                let mut device: Option<ID3D11Device> = None;
                let mut context: Option<ID3D11DeviceContext> = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )?;
                let device = device.expect("device");
                let context = context.expect("context");
                let device1: ID3D11Device1 = device.cast()?;
                let shared: ID3D11Texture2D =
                    device1.OpenSharedResource1(HANDLE(handle as *mut core::ffi::c_void))?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                shared.GetDesc(&mut desc);
                assert_eq!(desc.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
                assert_eq!((desc.Width, desc.Height), (64, 32));
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    ..desc
                };
                let mut staging: Option<ID3D11Texture2D> = None;
                device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
                let staging = staging.expect("staging");
                context.CopyResource(&staging, &shared);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let bytes = std::slice::from_raw_parts(mapped.pData as *const u8, 4);
                let pixel = [bytes[0], bytes[1], bytes[2], bytes[3]];
                context.Unmap(&staging, 0);
                Ok(pixel)
            }
        }
    }
}
