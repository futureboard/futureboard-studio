//! Offscreen WGPU arrangement renderer (scaffold).
//!
//! Renders into a private `wgpu::Texture` — **not** a competing window surface.
//! Compositing into GPUI still requires Blade/GPUI texture interop.
//!
//! GPU preference is configurable so AMD iGPU / Intel iGPU / older laptop
//! GPUs aren't forced into the HighPerformance adapter slot (which on
//! hybrid systems can fail device creation outright). On adapter or device
//! failure we never panic — `render_arrangement` falls back to the GPUI
//! paint renderer.

use super::renderer::{TimelineRenderOutput, TimelineRenderer};
use super::snapshot::TimelineRenderSnapshot;

/// User-selectable GPU preference for the offscreen timeline renderer.
/// Drives both `PowerPreference` and the fallback-adapter retry path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineGpuPreference {
    /// No strong preference — let the driver pick. Best for iGPU/AMD/Intel.
    Auto,
    /// Prefer the low-power adapter. Hint to use the integrated GPU.
    LowPower,
    /// Prefer the discrete/high-performance adapter (legacy default).
    HighPerformance,
}

impl Default for TimelineGpuPreference {
    fn default() -> Self {
        TimelineGpuPreference::Auto
    }
}

impl TimelineGpuPreference {
    fn from_env() -> Self {
        match std::env::var("FUTUREBOARD_GPU_PREFERENCE")
            .map(|v| v.to_ascii_lowercase())
            .ok()
            .as_deref()
        {
            Some("lowpower") | Some("low-power") | Some("low") | Some("integrated") => {
                Self::LowPower
            }
            Some("highperformance")
            | Some("high-performance")
            | Some("high")
            | Some("discrete") => Self::HighPerformance,
            _ => Self::Auto,
        }
    }

    fn to_power(self) -> wgpu::PowerPreference {
        match self {
            // No `None` variant on `PowerPreference`; `LowPower` is the
            // conservative default that still lets the OS pick the iGPU
            // when present. Avoids the HighPerformance trap on hybrid
            // laptops where the discrete GPU isn't ready/available.
            TimelineGpuPreference::Auto => wgpu::PowerPreference::LowPower,
            TimelineGpuPreference::LowPower => wgpu::PowerPreference::LowPower,
            TimelineGpuPreference::HighPerformance => wgpu::PowerPreference::HighPerformance,
        }
    }
}

fn gpu_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_GPU_RENDERER_DEBUG").is_some())
}

/// Saved GPU device id from Settings. Set once at app startup; consumed
/// by `WgpuTimelineRenderer::new` when the renderer is first constructed.
/// Empty string sentinel == "Auto" (no preference).
static PREFERRED_DEVICE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Called at app startup with the saved GPU Device preference. `id == ""`
/// means Auto (default adapter selection). Subsequent calls are no-ops.
pub fn set_preferred_gpu_device_id(id: &str) {
    let _ = PREFERRED_DEVICE_ID.set(id.to_string());
}

/// Public summary of one detected GPU adapter. Used by the Settings UI
/// to populate the GPU Device combo. Stable `id` derived from
/// vendor/device/name so the saved preference is portable across
/// process restarts even if backend ordering changes.
#[derive(Debug, Clone)]
pub struct GpuDeviceInfo {
    pub id: String,
    pub name: String,
    pub backend: Option<String>,
    pub device_type: Option<String>,
    pub vendor_id: Option<u32>,
    pub device_id: Option<u32>,
}

/// The machine's GPU adapters, enumerated once per process.
///
/// Enumeration builds a Vulkan, DX12/Metal and GL instance and can take from
/// tens of milliseconds to seconds. It used to run on every render of the
/// Preferences → Performance page (and again for every frame its dropdown was
/// open), which is what made that page stutter. Adapters do not come and go
/// during a session in any way the renderer could act on — the choice only
/// applies at the next launch — so one answer per process is the right amount.
///
/// Blocks the caller until the first enumeration finishes; concurrent callers
/// wait on the same one rather than starting their own. UI code should use
/// [`cached_gpu_devices`] and never call this on the render path.
pub fn gpu_devices() -> std::sync::Arc<Vec<GpuDeviceInfo>> {
    GPU_DEVICES
        .get_or_init(|| std::sync::Arc::new(enumerate_gpu_devices()))
        .clone()
}

/// The enumerated adapters if [`gpu_devices`] has already run, without ever
/// blocking. `None` means "still detecting", not "no GPU".
pub fn cached_gpu_devices() -> Option<std::sync::Arc<Vec<GpuDeviceInfo>>> {
    GPU_DEVICES.get().cloned()
}

static GPU_DEVICES: std::sync::OnceLock<std::sync::Arc<Vec<GpuDeviceInfo>>> =
    std::sync::OnceLock::new();

/// Enumerate all GPU adapters visible to wgpu on the current machine.
/// A clone of the process-wide list — see [`gpu_devices`].
pub fn list_available_gpu_devices() -> Vec<GpuDeviceInfo> {
    gpu_devices().as_ref().clone()
}

/// Never panics — adapter enumeration is wrapped in `catch_unwind` so a
/// broken driver on one backend can't take down the settings dialog.
/// Returns an empty Vec when no GPU is detected.
fn enumerate_gpu_devices() -> Vec<GpuDeviceInfo> {
    let result = std::panic::catch_unwind(|| {
        let instance = wgpu::Instance::default();
        // wgpu 29: enumerate_adapters is async (returns Future<Output = Vec<_>>).
        let adapters: Vec<wgpu::Adapter> =
            pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
        adapters
            .into_iter()
            .map(|adapter| {
                let info = adapter.get_info();
                let id = format!(
                    "{:?}:{:x}:{:x}:{}",
                    info.backend, info.vendor, info.device, info.name
                );
                GpuDeviceInfo {
                    id,
                    name: info.name.clone(),
                    backend: Some(format!("{:?}", info.backend)),
                    device_type: Some(format!("{:?}", info.device_type)),
                    vendor_id: Some(info.vendor),
                    device_id: Some(info.device),
                }
            })
            .collect::<Vec<_>>()
    });
    match result {
        Ok(devices) => {
            if gpu_debug_enabled() {
                eprintln!("[gpu-renderer] enumerated {} adapter(s)", devices.len());
                for d in &devices {
                    eprintln!(
                        "[gpu-renderer]   id={} name={} backend={:?} type={:?}",
                        d.id, d.name, d.backend, d.device_type
                    );
                }
            }
            devices
        }
        Err(_) => {
            if gpu_debug_enabled() {
                eprintln!("[gpu-renderer] adapter enumeration panicked; returning empty list");
            }
            Vec::new()
        }
    }
}

/// Classify this machine from the adapters wgpu can see, for the UI's
/// render-cost profile (see [`crate::perf::GpuClass`]).
///
/// A discrete adapter is always enough. Apple Silicon is integrated silicon
/// but has the memory bandwidth the LowEnd / 60 Hz DisplaySync profile was
/// written *against* leaving free — so it is treated as capable rather than
/// low-end. Enumeration is the same call the Settings GPU list uses — a few
/// milliseconds, once, at startup — and a driver that cannot enumerate yields
/// `Unknown`, which never slows the UI down on a guess.
pub fn detect_gpu_class() -> crate::perf::GpuClass {
    // Shares the one process-wide enumeration with Settings and the startup
    // probe instead of building its own set of instances.
    let devices = gpu_devices();
    crate::perf::classify_gpu_adapters(devices.iter().map(|device| {
        (
            device_type_class(device.device_type.as_deref()),
            device.name.as_str(),
        )
    }))
}

/// `device_type` is stored as wgpu's `Debug` name (`"DiscreteGpu"`, …).
fn device_type_class(kind: Option<&str>) -> crate::perf::GpuDeviceKind {
    match kind {
        Some("DiscreteGpu") => crate::perf::GpuDeviceKind::Discrete,
        Some("IntegratedGpu") => crate::perf::GpuDeviceKind::Integrated,
        Some("VirtualGpu") => crate::perf::GpuDeviceKind::Virtual,
        Some("Cpu") => crate::perf::GpuDeviceKind::Cpu,
        _ => crate::perf::GpuDeviceKind::Other,
    }
}

/// GPU texture produced by an offscreen arrangement pass.
pub struct WgpuOffscreenFrame {
    pub width: u32,
    pub height: u32,
    pub format: wgpu::TextureFormat,
    /// Offscreen color target — keep alive until composited or dropped.
    pub texture: wgpu::Texture,
}

/// Color target format. Non-sRGB so the snapshot's theme colors land in the
/// texture with the same numeric values GPUI paints, keeping the two backends
/// visually identical once the texture is composited.
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Bytes per instance: `rect` (4 x f32) + `color` (4 x f32).
const QUAD_INSTANCE_SIZE: u64 = 32;

/// Every arrangement-surface primitive is an axis-aligned rectangle, so the
/// whole pass is one instanced draw. `rect` is `(x, y, w, h)` in physical
/// pixels with the origin at the top-left of the arrangement body.
const ARRANGEMENT_SHADER: &str = r#"
struct Globals {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;

struct Instance {
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: Instance) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let corner = corners[vertex_index];
    let pixel = instance.rect.xy + corner * instance.rect.zw;
    // Pixel space (y down) -> clip space (y up).
    let ndc = vec2<f32>(
        pixel.x / max(globals.viewport.x, 1.0) * 2.0 - 1.0,
        1.0 - pixel.y / max(globals.viewport.y, 1.0) * 2.0,
    );

    var out: VertexOutput;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = instance.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// Pipeline plus the buffers reused across frames.
struct ArrangementPipeline {
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    /// Instance capacity of `instances`, in instances (not bytes).
    capacity: u64,
}

/// Cached offscreen color target. Recreated only when the arrangement body
/// changes size, so steady-state rendering allocates nothing.
struct OffscreenTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

pub struct WgpuTimelineRenderer {
    instance: wgpu::Instance,
    preference: TimelineGpuPreference,
    /// User-selected GPU device id (matches `GpuDeviceInfo::id`). `None`
    /// means Auto — let `request_adapter` pick.
    selected_device_id: Option<String>,
    device: Option<wgpu::Device>,
    queue: Option<wgpu::Queue>,
    max_texture_dimension_2d: u32,
    init_error: Option<String>,
    pipeline: Option<ArrangementPipeline>,
    target: Option<OffscreenTarget>,
    /// Scratch instance bytes, reused so building a frame's quads does not
    /// allocate once the buffer has grown to its working size.
    instance_bytes: Vec<u8>,
}

impl WgpuTimelineRenderer {
    pub fn new() -> Self {
        let preference = TimelineGpuPreference::from_env();
        let selected_device_id = PREFERRED_DEVICE_ID.get().cloned().filter(|s| !s.is_empty());
        Self::with_preference_and_device(preference, selected_device_id)
    }

    pub fn with_preference(preference: TimelineGpuPreference) -> Self {
        Self::with_preference_and_device(preference, None)
    }

    pub fn with_preference_and_device(
        preference: TimelineGpuPreference,
        selected_device_id: Option<String>,
    ) -> Self {
        Self {
            instance: wgpu::Instance::default(),
            preference,
            selected_device_id,
            device: None,
            queue: None,
            max_texture_dimension_2d: wgpu::Limits::downlevel_defaults().max_texture_dimension_2d,
            init_error: None,
            pipeline: None,
            target: None,
            instance_bytes: Vec::new(),
        }
    }

    pub fn is_available(&mut self) -> bool {
        self.init_error.is_none() && self.ensure_device().is_ok()
    }

    fn request_adapter(&self, fallback: bool) -> Result<wgpu::Adapter, String> {
        pollster::block_on(self.instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: self.preference.to_power(),
            compatible_surface: None,
            force_fallback_adapter: fallback,
        }))
        .map_err(|_| {
            if fallback {
                "no WGPU adapter (including fallback)".to_string()
            } else {
                "no WGPU adapter".to_string()
            }
        })
    }

    fn ensure_device(&mut self) -> Result<(), String> {
        if self.device.is_some() {
            return Ok(());
        }
        if let Some(err) = &self.init_error {
            return Err(err.clone());
        }
        // 1. If the user picked a specific GPU Device, scan enumerated
        //    adapters and use the matching one. Falls through to Auto if
        //    that adapter is no longer present (e.g. eGPU unplugged).
        // 2. Otherwise — or on miss — try the preferred adapter via
        //    `request_adapter`.
        // 3. Final retry uses `force_fallback_adapter = true` so the
        //    software (CPU) adapter is taken before we declare defeat.
        let adapter = if let Some(saved_id) = self.selected_device_id.as_deref() {
            let adapters: Vec<wgpu::Adapter> =
                pollster::block_on(self.instance.enumerate_adapters(wgpu::Backends::all()));
            let mut matched: Option<wgpu::Adapter> = None;
            for adapter in adapters {
                let info = adapter.get_info();
                let id = format!(
                    "{:?}:{:x}:{:x}:{}",
                    info.backend, info.vendor, info.device, info.name
                );
                if id == saved_id {
                    if gpu_debug_enabled() {
                        eprintln!(
                            "[gpu-renderer] using saved adapter: id={id} name={:?}",
                            info.name
                        );
                    }
                    matched = Some(adapter);
                    break;
                }
            }
            match matched {
                Some(a) => a,
                None => {
                    if gpu_debug_enabled() {
                        eprintln!(
                            "[gpu-renderer] saved GPU device id {saved_id:?} not found among enumerated adapters; falling back to Auto"
                        );
                    }
                    self.request_adapter(false).or_else(|primary| {
                        if gpu_debug_enabled() {
                            eprintln!(
                                "[gpu-renderer] auto adapter failed ({primary}); retrying with fallback"
                            );
                        }
                        self.request_adapter(true)
                    })?
                }
            }
        } else {
            match self.request_adapter(false) {
                Ok(a) => a,
                Err(primary) => {
                    if gpu_debug_enabled() {
                        eprintln!(
                            "[gpu-renderer] primary adapter request failed ({primary}); retrying with fallback"
                        );
                    }
                    self.request_adapter(true).map_err(|e| {
                        let msg = format!("{primary}; fallback also failed: {e}");
                        self.init_error = Some(msg.clone());
                        msg
                    })?
                }
            }
        };

        if gpu_debug_enabled() {
            let info = adapter.get_info();
            eprintln!(
                "[gpu-renderer] adapter selected: name={:?} backend={:?} device_type={:?} vendor=0x{:x} device=0x{:x} preference={:?}",
                info.name,
                info.backend,
                info.device_type,
                info.vendor,
                info.device,
                self.preference
            );
        }

        // Start with downlevel defaults for broad compatibility, but request
        // the adapter's native 2D texture size. A maximized 4K timeline can be
        // wider than the downlevel 2048px cap even at 100% scale.
        let adapter_limits = adapter.limits();
        let mut limits = wgpu::Limits::downlevel_defaults();
        limits.max_texture_dimension_2d = adapter_limits.max_texture_dimension_2d;
        let max_texture_dimension_2d = limits.max_texture_dimension_2d;
        let device_result = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("futureboard-timeline"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }));
        let (device, queue) = match device_result {
            Ok(pair) => pair,
            Err(e) => {
                let msg = format!("device creation failed: {e}");
                if gpu_debug_enabled() {
                    eprintln!("[gpu-renderer] {msg}; falling back to GPUI paint");
                }
                self.init_error = Some(msg.clone());
                return Err(msg);
            }
        };

        self.max_texture_dimension_2d = max_texture_dimension_2d;
        self.device = Some(device);
        self.queue = Some(queue);
        Ok(())
    }

    fn render_offscreen(
        &mut self,
        snapshot: &TimelineRenderSnapshot,
    ) -> Result<WgpuOffscreenFrame, String> {
        self.ensure_device()?;

        let width = snapshot.viewport.width.max(1.0) as u32;
        let height = snapshot.viewport.height.max(1.0) as u32;
        let max_texture_dimension_2d = self.max_texture_dimension_2d;
        if width > max_texture_dimension_2d || height > max_texture_dimension_2d {
            return Err(format!(
                "viewport {}x{} exceeds WGPU texture limit {}; falling back to GPUI paint",
                width, height, max_texture_dimension_2d
            ));
        }
        let format = OFFSCREEN_FORMAT;

        // Build this frame's quads before touching the GPU, so a snapshot that
        // paints nothing still produces a correctly cleared target.
        let mut instance_bytes = std::mem::take(&mut self.instance_bytes);
        instance_bytes.clear();
        let instance_count =
            build_arrangement_instances(snapshot, width as f32, height as f32, &mut instance_bytes);

        self.ensure_target(width, height)?;
        self.ensure_pipeline(instance_count)?;
        let device = self.device.as_ref().expect("device");
        let queue = self.queue.as_ref().expect("queue");
        let target = self.target.as_ref().expect("target");
        let pipeline = self.pipeline.as_ref().expect("pipeline");

        queue.write_buffer(
            &pipeline.globals,
            0,
            &globals_bytes(width as f32, height as f32),
        );
        if instance_count > 0 {
            queue.write_buffer(&pipeline.instances, 0, &instance_bytes);
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("timeline-arrangement"),
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("timeline-arrangement-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(rgba_to_wgpu(
                            crate::theme::Colors::timeline_content_background(),
                        )),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if instance_count > 0 {
                pass.set_pipeline(&pipeline.pipeline);
                pass.set_bind_group(0, &pipeline.globals_bind_group, &[]);
                pass.set_vertex_buffer(
                    0,
                    pipeline
                        .instances
                        .slice(..instance_count * QUAD_INSTANCE_SIZE),
                );
                pass.draw(0..6, 0..instance_count as u32);
            }
        }

        queue.submit(Some(encoder.finish()));

        crate::perf::count("gpu_quad_instances", instance_count);
        if gpu_debug_enabled() {
            eprintln!(
                "[gpu-renderer] WgpuTimelineRenderer offscreen {}x{} quads={} grid={} shades={} clips={} waveform_handles={}",
                width,
                height,
                instance_count,
                snapshot.grid_lines.len(),
                snapshot.bar_shades.len(),
                snapshot.clips.len(),
                snapshot
                    .clips
                    .iter()
                    .filter(|c| c.waveform.is_some())
                    .count(),
            );
        }

        let texture = target.texture.clone();
        self.instance_bytes = instance_bytes;
        Ok(WgpuOffscreenFrame {
            width,
            height,
            format,
            texture,
        })
    }

    /// (Re)create the offscreen color target when the arrangement body resizes.
    fn ensure_target(&mut self, width: u32, height: u32) -> Result<(), String> {
        if self
            .target
            .as_ref()
            .is_some_and(|target| target.width == width && target.height == height)
        {
            return Ok(());
        }
        let device = self.device.as_ref().ok_or("device not initialized")?;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("timeline-offscreen"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.target = Some(OffscreenTarget {
            texture,
            view,
            width,
            height,
        });
        Ok(())
    }

    /// Build the pipeline once, then only grow the instance buffer when a frame
    /// needs more quads than the current capacity.
    fn ensure_pipeline(&mut self, instance_count: u64) -> Result<(), String> {
        let device = self.device.as_ref().ok_or("device not initialized")?;
        if self.pipeline.is_none() {
            self.pipeline = Some(create_arrangement_pipeline(device, instance_count.max(256)));
            return Ok(());
        }
        let pipeline = self.pipeline.as_mut().expect("pipeline");
        if instance_count > pipeline.capacity {
            // Grow geometrically so a steadily busier viewport does not
            // reallocate every frame.
            let capacity = instance_count.next_power_of_two();
            pipeline.instances = create_instance_buffer(device, capacity);
            pipeline.capacity = capacity;
        }
        Ok(())
    }
}

fn create_instance_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("timeline-quad-instances"),
        size: capacity * QUAD_INSTANCE_SIZE,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_arrangement_pipeline(device: &wgpu::Device, capacity: u64) -> ArrangementPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("timeline-arrangement-shader"),
        source: wgpu::ShaderSource::Wgsl(ARRANGEMENT_SHADER.into()),
    });

    let globals = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("timeline-arrangement-globals"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("timeline-arrangement-globals-layout"),
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

    let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("timeline-arrangement-globals-bind-group"),
        layout: &bind_group_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: globals.as_entire_binding(),
        }],
    });

    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("timeline-arrangement-layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });

    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("timeline-arrangement-pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: QUAD_INSTANCE_SIZE,
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
                ],
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: OFFSCREEN_FORMAT,
                // Straight (non-premultiplied) alpha, matching how GPUI blends
                // the same theme colors in the paint fallback.
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    ArrangementPipeline {
        pipeline,
        globals,
        globals_bind_group,
        instances: create_instance_buffer(device, capacity),
        capacity,
    }
}

fn globals_bytes(width: f32, height: f32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&width.to_ne_bytes());
    bytes[4..8].copy_from_slice(&height.to_ne_bytes());
    bytes
}

fn push_quad(bytes: &mut Vec<u8>, rect: [f32; 4], color: gpui::Rgba) {
    for value in rect {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
    for value in [color.r, color.g, color.b, color.a] {
        bytes.extend_from_slice(&value.to_ne_bytes());
    }
}

fn rgba_to_wgpu(color: gpui::Rgba) -> wgpu::Color {
    wgpu::Color {
        r: color.r as f64,
        g: color.g as f64,
        b: color.b as f64,
        a: color.a as f64,
    }
}

/// Serialize the arrangement surface's quads, in paint order.
///
/// This is deliberately the same primitive set the GPUI paint fallback draws
/// (`gpui_paint::paint_grid`): the surface owns the bar shades and the grid
/// behind the lanes, while clips, notes, and the playhead remain interactive
/// GPUI elements layered above it. Returns the instance count.
fn build_arrangement_instances(
    snapshot: &TimelineRenderSnapshot,
    width: f32,
    height: f32,
    bytes: &mut Vec<u8>,
) -> u64 {
    use crate::components::timeline::timeline_state::GridLineLevel;
    use crate::theme::Colors;

    let mut count = 0u64;
    for shade in &snapshot.bar_shades {
        if shade.width <= 0.0 || shade.x >= width || shade.x + shade.width <= 0.0 {
            continue;
        }
        push_quad(
            bytes,
            [shade.x, 0.0, shade.width, height],
            Colors::timeline_region_background(),
        );
        count += 1;
    }

    for line in &snapshot.grid_lines {
        if line.x < 0.0 || line.x >= width {
            continue;
        }
        let color = match line.level {
            GridLineLevel::Bar => Colors::timeline_grid_bar(),
            GridLineLevel::Beat => Colors::timeline_grid_major(),
            GridLineLevel::Sub => Colors::timeline_grid_minor(),
        };
        push_quad(bytes, [line.x, 0.0, 1.0, height], color);
        count += 1;
    }

    count
}

impl TimelineRenderer for WgpuTimelineRenderer {
    fn backend_name(&self) -> &'static str {
        "wgpu-offscreen"
    }

    fn render_arrangement(&mut self, snapshot: TimelineRenderSnapshot) -> TimelineRenderOutput {
        let _s = crate::perf::PerfScope::enter("WgpuTimelineRenderer");
        match self.render_offscreen(&snapshot) {
            Ok(frame) => TimelineRenderOutput::WgpuOffscreen(frame),
            Err(error) => {
                eprintln!("[gpu-renderer] offscreen render failed: {error}");
                super::gpui_paint::GpuiPaintTimelineRenderer::new().render_arrangement(snapshot)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::render::snapshot::{
        BarShadeSnapshot, GridLineSnapshot, PlayheadSnapshot, SelectionSnapshot, VisibleBeatRange,
        VisibleTrackRange,
    };
    use crate::components::timeline::render::viewport::TimelineViewport;
    use crate::components::timeline::timeline_state::GridLineLevel;

    fn test_snapshot(width: f32, height: f32) -> TimelineRenderSnapshot {
        let mut viewport = TimelineViewport::new(width, height, 1.0, 0.0, 0.0, 40.0, 80.0, 0.5);
        viewport.width = width;
        viewport.height = height;
        TimelineRenderSnapshot {
            viewport,
            bpm: 120.0,
            beats_per_bar: 4.0,
            time_signature_revision: 0,
            visible_tracks: VisibleTrackRange {
                start_index: 0,
                end_index: 0,
            },
            visible_beats: VisibleBeatRange {
                start_beat: 0.0,
                end_beat: 8.0,
            },
            lanes: Vec::new(),
            clips: Vec::new(),
            grid_lines: vec![GridLineSnapshot {
                x: 40.0,
                beat: 1.0,
                level: GridLineLevel::Bar,
            }],
            bar_shades: vec![BarShadeSnapshot {
                x: 0.0,
                width: 20.0,
                bar: 0,
            }],
            playhead: PlayheadSnapshot { beat: 0.0, x: 0.0 },
            selection: SelectionSnapshot {
                selected_track_id: None,
                selected_clip_ids: Vec::new(),
            },
            track_insert_y: None,
        }
    }

    fn quad_color(bytes: &[u8], index: usize) -> [f32; 4] {
        let base = index * QUAD_INSTANCE_SIZE as usize + 16;
        let mut out = [0.0f32; 4];
        for (i, slot) in out.iter_mut().enumerate() {
            let at = base + i * 4;
            *slot = f32::from_ne_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        }
        out
    }

    /// Instance building is pure CPU work, so it is asserted on every machine —
    /// including CI without a GPU.
    #[test]
    fn every_shade_and_grid_line_becomes_one_quad() {
        let snapshot = test_snapshot(200.0, 100.0);
        let mut bytes = Vec::new();
        let count = build_arrangement_instances(&snapshot, 200.0, 100.0, &mut bytes);
        assert_eq!(count, 2, "one bar shade + one grid line");
        assert_eq!(bytes.len() as u64, count * QUAD_INSTANCE_SIZE);
    }

    #[test]
    fn quads_outside_the_viewport_are_dropped() {
        let mut snapshot = test_snapshot(200.0, 100.0);
        snapshot.grid_lines = vec![
            GridLineSnapshot {
                x: -5.0,
                beat: 0.0,
                level: GridLineLevel::Bar,
            },
            GridLineSnapshot {
                x: 250.0,
                beat: 9.0,
                level: GridLineLevel::Bar,
            },
            GridLineSnapshot {
                x: 100.0,
                beat: 4.0,
                level: GridLineLevel::Beat,
            },
        ];
        snapshot.bar_shades = vec![
            BarShadeSnapshot {
                x: -40.0,
                width: 20.0,
                bar: -2,
            },
            BarShadeSnapshot {
                x: 400.0,
                width: 20.0,
                bar: 10,
            },
        ];
        let mut bytes = Vec::new();
        let count = build_arrangement_instances(&snapshot, 200.0, 100.0, &mut bytes);
        assert_eq!(count, 1, "only the on-screen grid line survives");
    }

    /// The wgpu path must be a visual drop-in for `gpui_paint`, so each grid
    /// level has to carry the same theme color that fallback paints.
    #[test]
    fn grid_levels_map_to_their_theme_colors() {
        let mut snapshot = test_snapshot(200.0, 100.0);
        snapshot.bar_shades.clear();
        snapshot.grid_lines = vec![
            GridLineSnapshot {
                x: 10.0,
                beat: 0.0,
                level: GridLineLevel::Bar,
            },
            GridLineSnapshot {
                x: 20.0,
                beat: 1.0,
                level: GridLineLevel::Beat,
            },
            GridLineSnapshot {
                x: 30.0,
                beat: 2.0,
                level: GridLineLevel::Sub,
            },
        ];
        let mut bytes = Vec::new();
        let count = build_arrangement_instances(&snapshot, 200.0, 100.0, &mut bytes);
        assert_eq!(count, 3);

        let expect = |c: gpui::Rgba| [c.r, c.g, c.b, c.a];
        assert_eq!(
            quad_color(&bytes, 0),
            expect(crate::theme::Colors::timeline_grid_bar())
        );
        assert_eq!(
            quad_color(&bytes, 1),
            expect(crate::theme::Colors::timeline_grid_major())
        );
        assert_eq!(
            quad_color(&bytes, 2),
            expect(crate::theme::Colors::timeline_grid_minor())
        );
    }

    /// End-to-end GPU check: render the snapshot offscreen, copy the texture
    /// back, and assert the pixels actually changed where a grid line was
    /// requested. Skipped (not failed) when the machine has no usable adapter,
    /// so this runs on developer machines without gating GPU-less CI.
    #[test]
    fn offscreen_pass_paints_the_grid_line() {
        let mut renderer = WgpuTimelineRenderer::new();
        if !renderer.is_available() {
            eprintln!("[gpu-renderer] no adapter available; skipping readback test");
            return;
        }
        let width = 64u32;
        let height = 16u32;
        let mut snapshot = test_snapshot(width as f32, height as f32);
        snapshot.bar_shades.clear();
        snapshot.grid_lines = vec![GridLineSnapshot {
            x: 10.0,
            beat: 0.0,
            level: GridLineLevel::Bar,
        }];

        let frame = renderer
            .render_offscreen(&snapshot)
            .expect("offscreen render");
        let pixels = read_back_rgba(&renderer, &frame);

        let at = |x: u32, y: u32| -> [u8; 4] {
            let i = ((y * width + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        let background = at(0, 0);
        assert_ne!(
            at(10, 8),
            background,
            "grid line column must differ from the cleared background"
        );
        assert_eq!(at(30, 8), background, "empty columns stay at clear color");
    }

    fn read_back_rgba(renderer: &WgpuTimelineRenderer, frame: &WgpuOffscreenFrame) -> Vec<u8> {
        let device = renderer.device.as_ref().expect("device");
        let queue = renderer.queue.as_ref().expect("queue");
        // Copy rows must be aligned to COPY_BYTES_PER_ROW_ALIGNMENT.
        let unpadded = frame.width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("timeline-readback"),
            size: (padded * frame.height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            frame.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(frame.height),
                },
            },
            wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        });
        let mapped = buffer.slice(..).get_mapped_range();
        let mut out = Vec::with_capacity((unpadded * frame.height) as usize);
        for row in 0..frame.height {
            let start = (row * padded) as usize;
            out.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }
        drop(mapped);
        buffer.unmap();
        out
    }
}
