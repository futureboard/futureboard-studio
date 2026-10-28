//! wgpu for the note canvas.
//!
//! The canvas is rebuilt as triangles ([`super::scene`]) and drawn off screen
//! by the visualizer's renderer (`components::visualizer::gpu`, the same
//! device and pipelines), then composited into GPUI through a shared D3D11
//! texture — or, where textures can't be shared, a read-back image.
//!
//! Without the `gpu-renderer` feature (the plug-in build) or without a usable
//! adapter, [`GpuCanvas::paint`] returns false and the caller paints with
//! GPUI instead, so the editor always shows its notes.

use gpui::{App, Bounds, Pixels, Window};

use super::paint::{CanvasData, Palette};

#[cfg(feature = "gpu-renderer")]
pub use imp::GpuCanvas;

#[cfg(not(feature = "gpu-renderer"))]
#[derive(Default)]
pub struct GpuCanvas;

#[cfg(not(feature = "gpu-renderer"))]
impl GpuCanvas {
    pub fn paint(
        &mut self,
        _data: &CanvasData,
        _pal: &Palette,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> bool {
        false
    }

    pub fn active(&self) -> bool {
        false
    }
}

#[cfg(feature = "gpu-renderer")]
mod imp {
    use std::sync::Arc;

    use gpui::{point, size, Corners, DevicePixels, RenderImage};
    use smallvec::SmallVec;

    use super::*;
    use crate::components::visualizer::gpu::{self, Presented, Surface};
    use crate::components::visualizer::scene::{Scene, Vertex};

    #[derive(Default)]
    pub struct GpuCanvas {
        surface: Option<Surface>,
        /// Set once the GPU path failed; the canvas stays on GPUI after.
        failed: bool,
        /// Shared textures failed to import once: read back instead.
        readback_only: bool,
        #[cfg(target_os = "windows")]
        external: Option<Arc<gpui::D3D11ExternalImage>>,
        image: Option<Arc<RenderImage>>,
        stale: Vec<Arc<RenderImage>>,
    }

    impl GpuCanvas {
        /// Whether frames are coming from wgpu.
        pub fn active(&self) -> bool {
            self.surface.is_some() && !self.failed
        }

        /// Draws `data` into `bounds`. False: the caller paints instead.
        pub fn paint(
            &mut self,
            data: &CanvasData,
            pal: &Palette,
            bounds: Bounds<Pixels>,
            window: &mut Window,
            cx: &mut App,
        ) -> bool {
            if self.failed || std::env::var_os("VOXTUNE_NO_GPU").is_some() {
                return false;
            }
            for image in self.stale.drain(..) {
                cx.drop_image(image, Some(window));
            }
            let scale = window.scale_factor();
            let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
            let (pw, ph) = (
                (w * scale).round().max(1.0) as u32,
                (h * scale).round().max(1.0) as u32,
            );
            if self.surface.is_none() {
                #[cfg(target_os = "windows")]
                let luid = window.d3d11_adapter_luid();
                #[cfg(not(target_os = "windows"))]
                let luid = None;
                match gpu::context(luid).and_then(|context| {
                    let shared = context.shares_textures() && !self.readback_only;
                    Surface::new(context, pw, ph, shared)
                }) {
                    Ok(surface) => {
                        if std::env::var_os("VOXTUNE_GPU_DEBUG").is_some() {
                            eprintln!("[voxtune] note canvas on wgpu ({pw}x{ph})");
                        }
                        self.surface = Some(surface)
                    }
                    Err(error) => return self.fail(&error),
                }
            }
            let Some(surface) = self.surface.as_mut() else {
                return false;
            };
            if let Err(error) = surface.resize(pw, ph) {
                return self.fail(&error);
            }

            let built = super::super::scene::build(data, pal, w, h, scale);
            let frame = Scene {
                clear: Some(built.clear),
                fade: None,
                spectrogram: None,
                triangles: built
                    .vertices
                    .iter()
                    .map(|v| Vertex {
                        position: v.position,
                        color: v.color,
                    })
                    .collect(),
                glow: Vec::new(),
            };
            let presented = match surface.render(&frame) {
                Ok(p) => p,
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
                            let _ = window.paint_d3d11_external_image(
                                bounds,
                                Corners::default(),
                                external,
                            );
                            true
                        }
                        Err(error) => {
                            // Next frame reads back instead; this one falls
                            // back to GPUI.
                            eprintln!("[voxtune] shared texture import failed: {error}");
                            self.readback_only = true;
                            self.surface = None;
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
            eprintln!("[voxtune] GPU canvas unavailable, drawing with GPUI: {error}");
            self.failed = true;
            self.surface = None;
            false
        }
    }
}
