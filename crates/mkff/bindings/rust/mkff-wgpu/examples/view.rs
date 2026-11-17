//! Decode a tiny HEVC Annex-B clip with MKFF software fallback, upload NV12
//! planes into wgpu textures, and display them with a YUV→RGB fullscreen pass.
//!
//! ```sh
//! cargo run -p mkff-wgpu --example view
//! cargo run -p mkff-wgpu --example view -- path/to/clip.hevc
//! ```
//!
//! Close the window or press Escape to quit. This path uses CPU plane upload
//! (portable). Hardware zero-copy into wgpu is not wired yet.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use mkff::{Context, PixelFormat, ReceiveOutcome, VideoBackend};
use mkff_wgpu::{upload_nv12, Nv12Host};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

const VERTICES: [Vertex; 4] = [
    Vertex { pos: [-1.0, -1.0], uv: [0.0, 1.0] },
    Vertex { pos: [1.0, -1.0], uv: [1.0, 1.0] },
    Vertex { pos: [-1.0, 1.0], uv: [0.0, 0.0] },
    Vertex { pos: [1.0, 1.0], uv: [1.0, 0.0] },
];

const INDICES: [u16; 6] = [0, 1, 2, 2, 1, 3];

const SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_uv: texture_2d<f32>;

// BT.709 limited-range YUV → RGB (typical for HD HEVC).
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(tex_y, samp, in.uv).r;
    let chroma = textureSample(tex_uv, samp, in.uv).rg;
    let u = chroma.r - 0.5;
    let v = chroma.g - 0.5;
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    return vec4<f32>(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

fn main() {
    let input = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_hevc_path);

    let host = decode_first_nv12(&input).unwrap_or_else(|e| {
        eprintln!("decode failed for {}: {e}", input.display());
        std::process::exit(1);
    });

    println!(
        "decoded {}x{} NV12 from {} ({} Y bytes, {} UV bytes)",
        host.width,
        host.height,
        input.display(),
        host.y.len(),
        host.uv.len()
    );

    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        host,
        window: None,
        state: None,
    };
    event_loop.run_app(&mut app).expect("run_app");
}

fn default_hevc_path() -> PathBuf {
    // bindings/rust/mkff-wgpu → repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("testdata/tiny_main_256x144.hevc")
}

fn decode_first_nv12(path: &Path) -> Result<Nv12Host, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    let ctx = Context::new().map_err(|e| format!("context: {e}"))?;
    let mut decoder = ctx
        .video_decoder_hevc_with_backend(8, VideoBackend::MKFF_VIDEO_BACKEND_SOFTWARE_ONLY)
        .map_err(|e| format!("decoder create (software): {e}"))?;

    decoder
        .submit(&bytes, Some(0), Some(0))
        .map_err(|e| format!("submit: {e}"))?;
    decoder.flush().map_err(|e| format!("flush: {e}"))?;

    for _ in 0..64 {
        match decoder.receive().map_err(|e| format!("receive: {e}"))? {
            ReceiveOutcome::Frame(frame) => {
                let info = frame.info().map_err(|e| format!("frame info: {e}"))?;
                if info.format != PixelFormat::MKFF_PIXEL_FORMAT_NV12 {
                    return Err(format!("expected NV12, got {:?}", info.format));
                }
                let planes = frame
                    .map_cpu_planes()
                    .map_err(|e| format!("map_cpu_planes: {e}"))?;
                return Nv12Host::from_cpu_planes(&planes).map_err(|e| e.to_string());
            }
            ReceiveOutcome::NotReady => continue,
            ReceiveOutcome::EndOfStream => break,
        }
    }
    Err("no frame produced (bitstream may lack VCL NALs)".into())
}

struct App {
    host: Nv12Host,
    window: Option<Arc<Window>>,
    state: Option<GpuState>,
}

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    // Kept so the bind group’s texture handles stay valid.
    _y_tex: Option<wgpu::Texture>,
    _uv_tex: Option<wgpu::Texture>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("MKFF × wgpu — HEVC NV12")
            .with_inner_size(winit::dpi::LogicalSize::new(
                self.host.width.max(320),
                self.host.height.max(180),
            ));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let state = pollster::block_on(GpuState::new(window.clone(), &self.host));
        self.window = Some(window);
        self.state = Some(state);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. }
                if event.state.is_pressed()
                    && matches!(event.logical_key, Key::Named(NamedKey::Escape)) =>
            {
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(state) = self.state.as_mut() {
                    state.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(state) = self.state.as_mut() {
                    if let Err(e) = state.render() {
                        eprintln!("render error: {e}");
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl GpuState {
    async fn new(window: Arc<Window>, host: &Nv12Host) -> Self {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .expect("create_surface");

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("no wgpu adapter");

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("mkff-wgpu"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .expect("request_device");

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut y_tex = None;
        let mut uv_tex = None;
        upload_nv12(&device, &queue, host, &mut y_tex, &mut uv_tex);
        let y_view = y_tex.as_ref().unwrap().create_view(&Default::default());
        let uv_view = uv_tex.as_ref().unwrap().create_view(&Default::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nv12-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
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
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12-bg"),
            layout: &bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv2rgb"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv-pl"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-vbo"),
            contents: bytemuck::cast_slice(&VERTICES),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("quad-ibo"),
            contents: bytemuck::cast_slice(&INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group,
            vertex_buf,
            index_buf,
            _y_tex: y_tex,
            _uv_tex: uv_tex,
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let frame = self.surface.get_current_texture()?;
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("yuv-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.vertex_buf.slice(..));
            pass.set_index_buffer(self.index_buf.slice(..), wgpu::IndexFormat::Uint16);
            pass.draw_indexed(0..INDICES.len() as u32, 0, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}
