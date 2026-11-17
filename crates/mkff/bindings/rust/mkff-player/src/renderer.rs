//! GPU video output: Y / Cb / Cr planes (or RGBA) uploaded as textures, converted to RGB in a
//! fragment shader, drawn through an egui paint callback.

use std::sync::Arc;

use eframe::egui_wgpu::{self, wgpu};
use mkff_player::{yuv_to_rgb_matrix, Picture, PictureData};

const SHADER: &str = r#"
struct Params {
    r: vec4<f32>,
    g: vec4<f32>,
    b: vec4<f32>,
    // 0: planar Y'CbCr, 1: RGBA in tex_y
    mode: u32,
    // the render target is sRGB: output linear light
    srgb_target: u32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex_y: texture_2d<f32>;
@group(0) @binding(3) var tex_u: texture_2d<f32>;
@group(0) @binding(4) var tex_v: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle covering the viewport (egui sets it to the callback rect).
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(tex_y, samp, in.uv);
    let u = textureSample(tex_u, samp, in.uv).r;
    let v = textureSample(tex_v, samp, in.uv).r;
    var rgb: vec3<f32>;
    if (params.mode == 0u) {
        let p = vec4<f32>(y.r, u, v, 1.0);
        rgb = vec3<f32>(dot(params.r, p), dot(params.g, p), dot(params.b, p));
    } else {
        rgb = y.rgb;
    }
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (params.srgb_target == 1u) {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    r: [f32; 4],
    g: [f32; 4],
    b: [f32; 4],
    mode: u32,
    srgb_target: u32,
    _pad: [u32; 2],
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TexKey {
    rgba: bool,
    width: u32,
    height: u32,
    chroma_width: u32,
    chroma_height: u32,
}

struct Textures {
    key: TexKey,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Lives in egui-wgpu's callback resources.
struct Gpu {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    srgb_target: bool,
    textures: Option<Textures>,
    serial: u64,
}

/// Description of the GPU for the statistics overlay.
pub fn adapter_summary(rs: &egui_wgpu::RenderState) -> String {
    let info = rs.adapter.get_info();
    format!("wgpu {:?} · {}", info.backend, info.name)
}

/// Create the pipeline. Call once with eframe's render state.
pub fn init(rs: &egui_wgpu::RenderState) {
    let device = &rs.device;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("mkff-video"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    };
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("mkff-video"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
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
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            tex_entry(2),
            tex_entry(3),
            tex_entry(4),
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("mkff-video"),
        bind_group_layouts: &[&layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("mkff-video"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: rs.target_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("mkff-video"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("mkff-video"),
        size: std::mem::size_of::<Params>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    rs.renderer.write().callback_resources.insert(Gpu {
        pipeline,
        layout,
        sampler,
        uniform,
        srgb_target: rs.target_format.is_srgb(),
        textures: None,
        serial: 0,
    });
}

impl Gpu {
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, pic: &Picture) {
        if self.serial == pic.serial && self.textures.is_some() {
            return;
        }
        let key = match &pic.data {
            PictureData::Yuv {
                chroma_width,
                chroma_height,
                ..
            } => TexKey {
                rgba: false,
                width: pic.width,
                height: pic.height,
                chroma_width: *chroma_width,
                chroma_height: *chroma_height,
            },
            PictureData::Rgba(_) => TexKey {
                rgba: true,
                width: pic.width,
                height: pic.height,
                chroma_width: 1,
                chroma_height: 1,
            },
        };
        if key.width == 0 || key.height == 0 {
            return;
        }
        if self.textures.as_ref().is_none_or(|t| t.key != key) {
            self.textures = Some(self.create_textures(device, key));
        }
        let t = self.textures.as_ref().expect("textures created above");
        match &pic.data {
            PictureData::Yuv { y, u, v, .. } => {
                write_plane(queue, &t.y, y, key.width, key.height, 1);
                write_plane(queue, &t.u, u, key.chroma_width, key.chroma_height, 1);
                write_plane(queue, &t.v, v, key.chroma_width, key.chroma_height, 1);
            }
            PictureData::Rgba(px) => write_plane(queue, &t.y, px, key.width, key.height, 4),
        }
        let m = yuv_to_rgb_matrix(pic.matrix, pic.full_range);
        let params = Params {
            r: m[0],
            g: m[1],
            b: m[2],
            mode: u32::from(key.rgba),
            srgb_target: u32::from(self.srgb_target),
            _pad: [0; 2],
        };
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&params));
        self.serial = pic.serial;
    }

    fn create_textures(&self, device: &wgpu::Device, key: TexKey) -> Textures {
        let make = |w: u32, h: u32, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("mkff-video-plane"),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let (y, u, v) = if key.rgba {
            (
                make(key.width, key.height, wgpu::TextureFormat::Rgba8Unorm),
                make(1, 1, wgpu::TextureFormat::R8Unorm),
                make(1, 1, wgpu::TextureFormat::R8Unorm),
            )
        } else {
            (
                make(key.width, key.height, wgpu::TextureFormat::R8Unorm),
                make(
                    key.chroma_width,
                    key.chroma_height,
                    wgpu::TextureFormat::R8Unorm,
                ),
                make(
                    key.chroma_width,
                    key.chroma_height,
                    wgpu::TextureFormat::R8Unorm,
                ),
            )
        };
        let views = [&y, &u, &v].map(|t| t.create_view(&Default::default()));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mkff-video"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
            ],
        });
        Textures {
            key,
            y,
            u,
            v,
            bind_group,
        }
    }
}

fn write_plane(queue: &wgpu::Queue, tex: &wgpu::Texture, data: &[u8], w: u32, h: u32, bpp: u32) {
    let needed = (w * h * bpp) as usize;
    if data.len() < needed {
        log::warn!("plane too small: {} < {needed}", data.len());
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &data[..needed],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * bpp),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}

/// Paint callback: uploads `picture` if it is new, then draws it into the callback rect.
pub struct VideoCallback {
    pub picture: Arc<Picture>,
}

impl egui_wgpu::CallbackTrait for VideoCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(gpu) = resources.get_mut::<Gpu>() {
            gpu.upload(device, queue, &self.picture);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: eframe::egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(gpu) = resources.get::<Gpu>() else {
            return;
        };
        let Some(t) = &gpu.textures else { return };
        pass.set_pipeline(&gpu.pipeline);
        pass.set_bind_group(0, &t.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
