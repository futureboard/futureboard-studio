//! wgpu renderer for the 3D Room Preview.
//!
//! Draws a [`RoomMesh`] with a perspective camera, a depth buffer and 4×
//! multisampling into an off-screen target on the visualizers' device
//! (`components::visualizer::gpu`), then hands it to GPUI the same way: a
//! shared D3D11 texture on Windows, read-back pixels elsewhere. Opaque
//! triangles write depth; translucent ones (shadows, halos, the ear plane)
//! are blended over them, back to front, without writing it.
//!
//! Without a usable adapter [`Room3dCanvas::paint`] returns false and the
//! window draws its flat room instead, saying so.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{point, size, App, Bounds, Corners, DevicePixels, Pixels, RenderImage, Window};
use smallvec::SmallVec;

use super::{Mat4, RoomMesh, MESH_VERTEX_FLOATS};
use crate::components::visualizer::gpu::{self, GpuContext, Presented, Target, TARGET_FORMAT};

const SAMPLES: u32 = 4;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const VERTEX_STRIDE: u64 = (MESH_VERTEX_FLOATS * 4) as u64;

const SHADER: &str = r#"
struct Globals {
    view_proj: mat4x4<f32>,
    light: vec4<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec4<f32>,
) -> Out {
    var out: Out;
    out.position = globals.view_proj * vec4<f32>(position, 1.0);
    out.normal = normal;
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: Out) -> @location(0) vec4<f32> {
    // A zero normal is a line: drawn in its own colour, unlit.
    let len = length(in.normal);
    var shade = 1.0;
    if (len > 0.5) {
        let n = in.normal / len;
        let key = max(dot(n, normalize(globals.light.xyz)), 0.0);
        let sky = max(n.y, 0.0);
        shade = 0.38 + 0.52 * key + 0.16 * sky;
    }
    return vec4<f32>(in.color.rgb * shade, in.color.a);
}
"#;

struct Renderer {
    context: Rc<GpuContext>,
    target: Target,
    msaa: wgpu::TextureView,
    depth: wgpu::TextureView,
    opaque: wgpu::RenderPipeline,
    translucent: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    vertices: wgpu::Buffer,
    vertex_capacity: u64,
    bytes: Vec<u8>,
    allow_shared: bool,
}

fn attachment(
    device: &wgpu::Device,
    label: &str,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: SAMPLES,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn vertex_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("room3d-vertices"),
        size: capacity * VERTEX_STRIDE,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

impl Renderer {
    fn new(
        context: Rc<GpuContext>,
        width: u32,
        height: u32,
        allow_shared: bool,
    ) -> Result<Self, String> {
        let device = context.device();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("room3d-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("room3d-globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("room3d"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: VERTEX_STRIDE,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 12,
                    shader_location: 1,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x4,
                    offset: 24,
                    shader_location: 2,
                },
            ],
        };
        // The target's alpha stays 1 so GPUI composites an opaque image.
        let blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::Zero,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let pipeline = |label: &str, depth_write: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
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
                        format: TARGET_FORMAT,
                        blend: Some(blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                // Both windings: lines and discs are seen from either side.
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: SAMPLES,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview_mask: None,
                cache: None,
            })
        };
        let opaque = pipeline("room3d-opaque", true);
        let translucent = pipeline("room3d-translucent", false);
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("room3d-globals"),
            size: 80,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("room3d-globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let (width, height) = (width.max(1), height.max(1));
        let target = gpu::create_target(&context, width, height, allow_shared)?;
        let msaa = attachment(device, "room3d-msaa", TARGET_FORMAT, width, height);
        let depth = attachment(device, "room3d-depth", DEPTH_FORMAT, width, height);
        let vertex_capacity = 32_768;
        let vertices = vertex_buffer(device, vertex_capacity);
        Ok(Self {
            context,
            target,
            msaa,
            depth,
            opaque,
            translucent,
            globals,
            globals_bind,
            vertices,
            vertex_capacity,
            bytes: Vec::new(),
            allow_shared,
        })
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        let (width, height) = (width.max(1), height.max(1));
        if self.target.size() == (width, height) {
            return Ok(());
        }
        let device = self.context.device();
        self.target = gpu::create_target(&self.context, width, height, self.allow_shared)?;
        self.msaa = attachment(device, "room3d-msaa", TARGET_FORMAT, width, height);
        self.depth = attachment(device, "room3d-depth", DEPTH_FORMAT, width, height);
        Ok(())
    }

    fn render(&mut self, mesh: &RoomMesh, view_proj: &Mat4) -> Result<Presented, String> {
        let context = self.context.clone();
        let mut globals = [0u8; 80];
        for (i, value) in view_proj.iter().flatten().enumerate() {
            globals[i * 4..i * 4 + 4].copy_from_slice(&value.to_ne_bytes());
        }
        // Key light from above, front-right of the listener.
        for (i, value) in [0.45f32, 0.85, -0.3, 0.0].iter().enumerate() {
            globals[64 + i * 4..68 + i * 4].copy_from_slice(&value.to_ne_bytes());
        }
        context.queue().write_buffer(&self.globals, 0, &globals);

        self.bytes.clear();
        for vertex in mesh.opaque.iter().chain(&mesh.translucent) {
            for value in vertex
                .position
                .iter()
                .chain(&vertex.normal)
                .chain(&vertex.color)
            {
                self.bytes.extend_from_slice(&value.to_ne_bytes());
            }
        }
        let count = self.bytes.len() as u64 / VERTEX_STRIDE;
        if count > self.vertex_capacity {
            self.vertex_capacity = count.next_power_of_two();
            self.vertices = vertex_buffer(context.device(), self.vertex_capacity);
        }
        if !self.bytes.is_empty() {
            context.queue().write_buffer(&self.vertices, 0, &self.bytes);
        }

        let mut encoder =
            context
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("room3d-frame"),
                });
        {
            let [r, g, b, _] = mesh.clear;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("room3d-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa,
                    depth_slice: None,
                    resolve_target: Some(self.target.view()),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: r as f64,
                            g: g as f64,
                            b: b as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Discard,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.globals_bind, &[]);
            if count > 0 {
                pass.set_vertex_buffer(0, self.vertices.slice(..count * VERTEX_STRIDE));
            }
            let opaque = mesh.opaque.len() as u32;
            let translucent = mesh.translucent.len() as u32;
            if opaque > 0 {
                pass.set_pipeline(&self.opaque);
                pass.draw(0..opaque, 0..1);
            }
            if translucent > 0 {
                pass.set_pipeline(&self.translucent);
                pass.draw(opaque..opaque + translucent, 0..1);
            }
        }
        gpu::present(&context, &self.target, encoder)
    }
}

/// The room view's GPU half: draws a mesh into its bounds through GPUI, or
/// says it cannot.
#[derive(Default)]
pub struct Room3dCanvas {
    renderer: Option<Renderer>,
    /// The GPU path failed: the view stays flat from here.
    failed: bool,
    readback_only: bool,
    #[cfg(target_os = "windows")]
    external: Option<Arc<gpui::D3D11ExternalImage>>,
    image: Option<Arc<RenderImage>>,
    stale: Vec<Arc<RenderImage>>,
}

impl Room3dCanvas {
    /// Whether the GPU path has given up on this machine.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Draw `mesh` seen through `view_proj` into `bounds`. False: nothing was
    /// drawn and the caller paints instead.
    pub fn paint(
        &mut self,
        mesh: &RoomMesh,
        view_proj: &Mat4,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if self.failed {
            return false;
        }
        if std::env::var_os("FUTUREBOARD_ROOM3D_NO_GPU").is_some() {
            self.failed = true;
        }
        let drawn = !self.failed && self.paint_gpu(mesh, view_proj, bounds, window, cx);
        if self.failed {
            // The view says why it is flat from the next frame on.
            window.refresh();
        }
        drawn
    }

    fn paint_gpu(
        &mut self,
        mesh: &RoomMesh,
        view_proj: &Mat4,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        for image in self.stale.drain(..) {
            cx.drop_image(image, Some(window));
        }
        let scale = window.scale_factor();
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let (pw, ph) = (
            (w * scale).round().max(1.0) as u32,
            (h * scale).round().max(1.0) as u32,
        );
        if self.renderer.is_none() {
            #[cfg(target_os = "windows")]
            let luid = window.d3d11_adapter_luid();
            #[cfg(not(target_os = "windows"))]
            let luid = None;
            match gpu::context(luid).and_then(|context| {
                let shared = context.shares_textures() && !self.readback_only;
                Renderer::new(context, pw, ph, shared)
            }) {
                Ok(renderer) => self.renderer = Some(renderer),
                Err(error) => return self.fail(&error),
            }
        }
        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };
        if let Err(error) = renderer.resize(pw, ph) {
            return self.fail(&error);
        }
        let presented = match renderer.render(mesh, view_proj) {
            Ok(presented) => presented,
            Err(error) => return self.fail(&error),
        };
        match presented {
            #[cfg(target_os = "windows")]
            Presented::Shared {
                handle,
                width,
                height,
            } => {
                let external = self
                    .external
                    .get_or_insert_with(|| window.create_d3d11_external_image())
                    .clone();
                match external.update_from_shared_texture(
                    handle,
                    point(DevicePixels(0), DevicePixels(0)),
                    size(DevicePixels(width as i32), DevicePixels(height as i32)),
                ) {
                    Ok(()) => {
                        let _ =
                            window.paint_d3d11_external_image(bounds, Corners::default(), external);
                        true
                    }
                    Err(error) => {
                        // Read back from the next frame; this one is flat.
                        eprintln!("[room3d] shared texture import failed: {error}");
                        self.readback_only = true;
                        self.renderer = None;
                        false
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            Presented::Shared { .. } => false,
            Presented::Pixels {
                bgra,
                width,
                height,
            } => {
                let Some(buffer) = image::ImageBuffer::from_raw(width, height, bgra) else {
                    return false;
                };
                let next = Arc::new(RenderImage::new(SmallVec::from_elem(
                    image::Frame::new(buffer),
                    1,
                )));
                if let Some(previous) = self.image.replace(next.clone()) {
                    self.stale.push(previous);
                }
                let _ = window.paint_image(bounds, Corners::default(), next, 0, false);
                true
            }
        }
    }

    fn fail(&mut self, error: &str) -> bool {
        eprintln!("[room3d] 3D view unavailable, drawing the room flat: {error}");
        self.failed = true;
        self.renderer = None;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::{build_mesh, OrbitCamera, RoomPalette, SceneSource};
    use super::*;

    /// No GPU on the test machine: nothing to check, the test passes.
    #[test]
    fn a_frame_draws_the_room_with_depth() {
        let context = match gpu::context(None) {
            Ok(context) => context,
            Err(error) => {
                eprintln!("[room3d-test] no GPU: {error}");
                return;
            }
        };
        let mut renderer = Renderer::new(context, 96, 64, false).expect("renderer");
        let camera = OrbitCamera::default();
        let palette = RoomPalette {
            background: [0.0, 0.0, 0.0, 1.0],
            floor: [0.2, 0.2, 0.2, 1.0],
            grid: [0.3, 0.3, 0.3, 1.0],
            edge: [0.4, 0.4, 0.4, 1.0],
            ear_plane: [1.0, 1.0, 1.0, 0.03],
            head: [0.6, 0.6, 0.6, 1.0],
            speaker: [0.5, 0.5, 0.5, 1.0],
            accent: [0.2, 0.8, 0.9, 1.0],
            shadow: [0.0, 0.0, 0.0, 0.35],
        };
        let source = SceneSource {
            heard: solfege_spatialaudio::SourceParams::default(),
            base: solfege_spatialaudio::SourceParams::default(),
            color: [1.0, 0.0, 0.0, 1.0],
            automated: false,
            selected: false,
        };
        let mesh = build_mesh(&camera, &palette, &[], &[source], false);
        let Presented::Pixels {
            bgra,
            width,
            height,
        } = renderer
            .render(&mesh, &camera.view_proj(96.0 / 64.0))
            .expect("frame")
        else {
            panic!("a readback target hands back pixels");
        };
        assert_eq!((width, height), (96, 64));
        // The red channel lands where the camera projects it.
        let (px, py, _) = camera
            .project(super::super::world(source.heard.position), 96.0, 64.0)
            .expect("visible");
        let i = ((py.round() as u32 * width + px.round() as u32) * 4) as usize;
        let [b, g, r, a] = [bgra[i], bgra[i + 1], bgra[i + 2], bgra[i + 3]];
        assert!(
            r > 60 && r > g * 2 && r > b * 2,
            "red sphere, got {:?}",
            [r, g, b]
        );
        assert_eq!(a, 255);
        // A corner of the frame is background or room, never transparent.
        assert_eq!(bgra[3], 255);
    }
}
