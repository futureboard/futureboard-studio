use collections::FxHashMap;
use etagere::BucketedAtlasAllocator;
use parking_lot::Mutex;
use windows::{
    Win32::{
        Foundation::HANDLE,
        Graphics::{
            Direct3D11::{
                D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
                D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11ShaderResourceView,
                ID3D11Texture2D,
            },
            Dxgi::Common::*,
        },
    },
    core::Interface,
};

use gpui::{
    AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTextureList, AtlasTile, Bounds, DevicePixels,
    PlatformAtlas, Point, Size,
    osr_profile::{self, Counter, Stage},
};

pub(crate) struct DirectXAtlas(Mutex<DirectXAtlasState>);

struct DirectXAtlasState {
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    monochrome_textures: AtlasTextureList<DirectXAtlasTexture>,
    polychrome_textures: AtlasTextureList<DirectXAtlasTexture>,
    subpixel_textures: AtlasTextureList<DirectXAtlasTexture>,
    tiles_by_key: FxHashMap<AtlasKey, AtlasTile>,
    /// Shared textures already opened on this device, keyed by the handle CEF
    /// reported. Chromium cycles a small pool of OSR surfaces, so without this
    /// every frame pays for a kernel-mode `OpenSharedResource1` and the
    /// deferred destruction of the wrapper it returns. The caller still reads
    /// `GetDesc` from the returned wrapper every frame, so a reused entry is
    /// validated against the surface it actually points at.
    shared_textures: FxHashMap<usize, ID3D11Texture2D>,
}

/// Upper bound on cached shared-texture opens. Chromium's OSR pool is a handful
/// of surfaces per browser; this is generous enough to never evict in steady
/// state and small enough that a leaked handle cannot grow without limit.
const MAX_CACHED_SHARED_TEXTURES: usize = 16;

struct DirectXAtlasTexture {
    id: AtlasTextureId,
    bytes_per_pixel: u32,
    allocator: BucketedAtlasAllocator,
    texture: ID3D11Texture2D,
    view: [Option<ID3D11ShaderResourceView>; 1],
    live_atlas_keys: u32,
}

impl DirectXAtlas {
    pub(crate) fn new(device: &ID3D11Device, device_context: &ID3D11DeviceContext) -> Self {
        DirectXAtlas(Mutex::new(DirectXAtlasState {
            device: device.clone(),
            device_context: device_context.clone(),
            monochrome_textures: Default::default(),
            polychrome_textures: Default::default(),
            subpixel_textures: Default::default(),
            tiles_by_key: Default::default(),
            shared_textures: Default::default(),
        }))
    }

    pub(crate) fn get_texture_view(
        &self,
        id: AtlasTextureId,
    ) -> [Option<ID3D11ShaderResourceView>; 1] {
        let lock = self.0.lock();
        let tex = lock.texture(id);
        tex.view.clone()
    }

    pub(crate) fn handle_device_lost(
        &self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
    ) {
        let mut lock = self.0.lock();
        lock.device = device.clone();
        lock.device_context = device_context.clone();
        lock.monochrome_textures = AtlasTextureList::default();
        lock.polychrome_textures = AtlasTextureList::default();
        lock.subpixel_textures = AtlasTextureList::default();
        lock.tiles_by_key.clear();
        // The cached sources belong to the device that just went away; keeping
        // them would hand the replacement device textures it cannot copy from.
        lock.shared_textures.clear();
    }
}

impl PlatformAtlas for DirectXAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> anyhow::Result<
            Option<(Size<DevicePixels>, std::borrow::Cow<'a, [u8]>)>,
        >,
    ) -> anyhow::Result<Option<AtlasTile>> {
        let mut lock = self.0.lock();
        if let Some(tile) = lock.tiles_by_key.get(key) {
            Ok(Some(*tile))
        } else {
            let Some((size, bytes)) = build()? else {
                return Ok(None);
            };
            let tile = lock
                .allocate(size, key.texture_kind())
                .ok_or_else(|| anyhow::anyhow!("failed to allocate"))?;
            let texture = lock.texture(tile.texture_id);
            texture.upload(&lock.device_context, tile.bounds, &bytes);
            lock.tiles_by_key.insert(key.clone(), tile);
            Ok(Some(tile))
        }
    }

    fn remove(&self, key: &AtlasKey) {
        let mut lock = self.0.lock();

        let Some(tile) = lock.tiles_by_key.remove(key) else {
            return;
        };
        let id = tile.texture_id;

        let textures = match id.kind {
            AtlasTextureKind::Monochrome => &mut lock.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut lock.polychrome_textures,
            AtlasTextureKind::Subpixel => &mut lock.subpixel_textures,
        };

        let Some(texture_slot) = textures.textures.get_mut(id.index as usize) else {
            return;
        };

        if let Some(mut texture) = texture_slot.take() {
            texture.deallocate(tile);
            if texture.is_unreferenced() {
                textures.free_list.push(texture.id.index as usize);
            } else {
                *texture_slot = Some(texture);
            }
        }
    }

    fn d3d11_adapter_luid(&self) -> Option<u64> {
        self.0.lock().adapter_luid().ok()
    }

    fn copy_d3d11_shared_texture(
        &self,
        key: &AtlasKey,
        shared_handle: usize,
        source_origin: Point<DevicePixels>,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<Option<AtlasTile>> {
        if shared_handle == 0 || size.width.0 <= 0 || size.height.0 <= 0 {
            anyhow::bail!("invalid CEF D3D11 shared texture");
        }
        if key.texture_kind() != AtlasTextureKind::Polychrome {
            anyhow::bail!("CEF shared textures require a polychrome atlas tile");
        }

        let mut lock = self.0.lock();
        let source = lock.open_shared_source(shared_handle)?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut source_desc) };
        lock.report_first_shared_texture(&source, &source_desc);
        if source_desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 != 0 {
            lock.shared_textures.remove(&shared_handle);
            anyhow::bail!(
                "CEF shared texture requires keyed-mutex synchronization, but no producer key protocol is available; software OSR fallback required"
            );
        }
        let source_right = source_origin.x.0.saturating_add(size.width.0);
        let source_bottom = source_origin.y.0.saturating_add(size.height.0);
        if source_origin.x.0 < 0
            || source_origin.y.0 < 0
            || source_right > source_desc.Width as i32
            || source_bottom > source_desc.Height as i32
            || source_desc.SampleDesc.Count != 1
        {
            // A cached open that no longer describes the surface CEF is talking
            // about is worse than no cache at all: forget it so the next frame
            // re-opens the handle rather than copying from a stale texture.
            lock.shared_textures.remove(&shared_handle);
            anyhow::bail!(
                "CEF shared texture metadata mismatch: source={}x{} rect=({}, {}) {}x{} samples={}",
                source_desc.Width,
                source_desc.Height,
                source_origin.x.0,
                source_origin.y.0,
                size.width.0,
                size.height.0,
                source_desc.SampleDesc.Count
            );
        }
        if !matches!(
            source_desc.Format,
            DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB
        ) {
            lock.shared_textures.remove(&shared_handle);
            anyhow::bail!(
                "unsupported CEF shared texture format: {:?}",
                source_desc.Format
            );
        }

        let tile = if let Some(tile) = lock.tiles_by_key.get(key).copied() {
            if tile.bounds.size != size {
                anyhow::bail!("CEF atlas tile size changed without a new image key");
            }
            tile
        } else {
            let tile = lock
                .allocate(size, AtlasTextureKind::Polychrome)
                .ok_or_else(|| anyhow::anyhow!("failed to allocate CEF atlas tile"))?;
            lock.tiles_by_key.insert(key.clone(), tile);
            osr_profile::count(Counter::TileAllocations, 1);
            tile
        };
        let destination = lock.texture(tile.texture_id).texture.clone();
        let context = lock.device_context.clone();
        drop(lock);

        unsafe {
            let _copy = osr_profile::span(Stage::TextureCopy);
            context.CopySubresourceRegion(
                &destination,
                0,
                tile.bounds.origin.x.0 as u32,
                tile.bounds.origin.y.0 as u32,
                0,
                &source,
                0,
                Some(&D3D11_BOX {
                    left: source_origin.x.0 as u32,
                    top: source_origin.y.0 as u32,
                    front: 0,
                    right: source_right as u32,
                    bottom: source_bottom as u32,
                    back: 1,
                }),
            );
        }
        // Submit the copy while CEF still owns the surface it reads from.
        //
        // `Flush` only *submits*; it does not wait, and it does not make the
        // cross-device read safe on its own. What it buys is that the copy is in
        // the GPU queue before Chromium hands the same pool surface to its next
        // frame. It also ends GPUI's current command batch, so it is measured
        // separately: on a producer running at the display's own rate this is
        // one extra submission per frame, and it is a candidate for removal if
        // the numbers say it costs more than it protects.
        unsafe {
            let _flush = osr_profile::span(Stage::TextureFlush);
            context.Flush();
        }
        Ok(Some(tile))
    }
}

impl DirectXAtlasState {
    /// Describe the first CEF surface this process ever imports.
    ///
    /// Everything here is fixed for the lifetime of a browser, so it is logged
    /// once rather than measured.
    ///
    /// A surface carrying `SHARED_KEYEDMUTEX` expects `AcquireSync`/`ReleaseSync`
    /// around every access. This importer does not know CEF's producer key
    /// protocol, so the caller rejects that surface and recreates the browser
    /// in software OSR instead of racing the producer.
    ///
    /// **Adapter identity is reported, not inferred.** D3D11 has no
    /// "cross-adapter" misc flag to test — that is a D3D12 heap property — and
    /// a plain D3D11 shared handle cannot be opened on a different adapter at
    /// all. So reaching this function already implies Chromium and GPUI agree
    /// on the adapter; what is logged is *which* one, to be compared against
    /// CEF's own GPU selection in `chrome://gpu`. A genuine mismatch surfaces
    /// as `OpenSharedResource1` failing, which the caller reports as a copy
    /// failure and which retires the browser to software OSR.
    fn report_first_shared_texture(&self, source: &ID3D11Texture2D, desc: &D3D11_TEXTURE2D_DESC) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static REPORTED: AtomicBool = AtomicBool::new(false);
        if REPORTED.swap(true, Ordering::Relaxed) {
            return;
        }

        let keyed_mutex = desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 != 0;
        let keyed_mutex_available = source
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex>()
            .is_ok();
        if keyed_mutex {
            log::warn!(
                "CEF shared texture has SHARED_KEYEDMUTEX; importer will use software fallback because producer synchronization keys are unknown"
            );
        }
        if !osr_profile::enabled()
            && std::env::args_os().skip(1).all(|arg| {
                !arg.to_string_lossy()
                    .eq_ignore_ascii_case("--gpu-diagnostics")
            })
            && std::env::var_os("FUTUREBOARD_GPU_DIAGNOSTICS").is_none()
        {
            return;
        }
        eprintln!(
            "[osr-gpu] first accelerated frame: {}x{} format={:?} usage={:?} bind_flags=0x{:X} samples={} misc_flags=0x{:X} \
             keyed_mutex={keyed_mutex} keyed_mutex_available={keyed_mutex_available} handle_type={} gpui_adapter={} \
             (compare against chrome://gpu's adapter for the CEF GPU process)",
            desc.Width,
            desc.Height,
            desc.Format,
            desc.Usage,
            desc.BindFlags,
            desc.SampleDesc.Count,
            desc.MiscFlags,
            if desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 as u32 != 0 {
                "NT"
            } else {
                "legacy"
            },
            self.adapter_identity(),
        );
        log::info!(
            "CEF shared texture metadata width={} height={} format={:?} usage={:?} bind_flags=0x{:X} misc_flags=0x{:X} keyed_mutex={keyed_mutex} keyed_mutex_available={keyed_mutex_available}",
            desc.Width,
            desc.Height,
            desc.Format,
            desc.Usage,
            desc.BindFlags,
            desc.MiscFlags
        );
    }

    /// The LUID of the adapter GPUI's device runs on, `(High << 32) | Low`.
    fn adapter_luid(&self) -> anyhow::Result<u64> {
        use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
        let dxgi_device: IDXGIDevice = self.device.cast()?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter()? };
        let desc = unsafe { adapter.GetDesc()? };
        Ok(((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64)
    }

    /// `LUID and description of the adapter GPUI's device runs on`, or a reason
    /// it could not be resolved. Diagnostics only.
    fn adapter_identity(&self) -> String {
        use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
        let resolve = || -> anyhow::Result<String> {
            let dxgi_device: IDXGIDevice = self.device.cast()?;
            let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter()? };
            let desc = unsafe { adapter.GetDesc()? };
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            Ok(format!(
                "LUID={:08X}:{:08X} \"{name}\"",
                desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart
            ))
        };
        resolve().unwrap_or_else(|error| format!("<unavailable: {error}>"))
    }

    /// The opened `ID3D11Texture2D` behind a CEF shared handle, reusing a
    /// previous open when one exists.
    ///
    /// `OpenSharedResource1` is a kernel-mode call that materialises a fresh COM
    /// wrapper; doing it inside every accelerated paint put a driver round-trip
    /// and a deferred destruction on the producer's critical path. Chromium
    /// cycles a small, stable pool of OSR surfaces, so caching by handle turns
    /// that into a one-time cost per surface.
    ///
    /// The residual risk is a recycled handle value naming a *different* surface
    /// of identical dimensions and format, which the caller's metadata check
    /// cannot distinguish. That requires CEF to close a pool surface and open a
    /// replacement mid-session, which it does on resize — where the atlas tile
    /// is rebuilt and the entry is evicted anyway. Set
    /// `FUTUREBOARD_OSR_NO_HANDLE_CACHE` to fall back to opening every frame.
    fn open_shared_source(&mut self, shared_handle: usize) -> anyhow::Result<ID3D11Texture2D> {
        if !handle_cache_enabled() {
            osr_profile::count(Counter::SharedHandleOpens, 1);
            let _open = osr_profile::span(Stage::TextureOpen);
            let device1: ID3D11Device1 = self.device.cast()?;
            return Ok(unsafe {
                device1.OpenSharedResource1(HANDLE(shared_handle as *mut core::ffi::c_void))?
            });
        }

        if let Some(cached) = self.shared_textures.get(&shared_handle) {
            osr_profile::count(Counter::SharedHandleCacheHits, 1);
            return Ok(cached.clone());
        }

        osr_profile::count(Counter::SharedHandleOpens, 1);
        let texture: ID3D11Texture2D = {
            let _open = osr_profile::span(Stage::TextureOpen);
            let device1: ID3D11Device1 = self.device.cast()?;
            unsafe { device1.OpenSharedResource1(HANDLE(shared_handle as *mut core::ffi::c_void))? }
        };

        // A pool larger than the cap means the assumption above is wrong for
        // this build of CEF; drop everything rather than grow without bound.
        if self.shared_textures.len() >= MAX_CACHED_SHARED_TEXTURES {
            self.shared_textures.clear();
        }
        self.shared_textures.insert(shared_handle, texture.clone());
        Ok(texture)
    }

    fn allocate(
        &mut self,
        size: Size<DevicePixels>,
        texture_kind: AtlasTextureKind,
    ) -> Option<AtlasTile> {
        {
            let textures = match texture_kind {
                AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
                AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
                AtlasTextureKind::Subpixel => &mut self.subpixel_textures,
            };

            if let Some(tile) = textures
                .iter_mut()
                .rev()
                .find_map(|texture| texture.allocate(size))
            {
                return Some(tile);
            }
        }

        let texture = self.push_texture(size, texture_kind)?;
        texture.allocate(size)
    }

    fn push_texture(
        &mut self,
        min_size: Size<DevicePixels>,
        kind: AtlasTextureKind,
    ) -> Option<&mut DirectXAtlasTexture> {
        const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
            width: DevicePixels(1024),
            height: DevicePixels(1024),
        };
        // Max texture size for DirectX. See:
        // https://learn.microsoft.com/en-us/windows/win32/direct3d11/overviews-direct3d-11-resources-limits
        const MAX_ATLAS_SIZE: Size<DevicePixels> = Size {
            width: DevicePixels(16384),
            height: DevicePixels(16384),
        };
        let size = min_size.min(&MAX_ATLAS_SIZE).max(&DEFAULT_ATLAS_SIZE);
        let pixel_format;
        let bind_flag;
        let bytes_per_pixel;
        match kind {
            AtlasTextureKind::Monochrome => {
                pixel_format = DXGI_FORMAT_R8_UNORM;
                bind_flag = D3D11_BIND_SHADER_RESOURCE;
                bytes_per_pixel = 1;
            }
            AtlasTextureKind::Polychrome => {
                pixel_format = DXGI_FORMAT_B8G8R8A8_UNORM;
                bind_flag = D3D11_BIND_SHADER_RESOURCE;
                bytes_per_pixel = 4;
            }
            AtlasTextureKind::Subpixel => {
                pixel_format = DXGI_FORMAT_R8G8B8A8_UNORM;
                bind_flag = D3D11_BIND_SHADER_RESOURCE;
                bytes_per_pixel = 4;
            }
        }
        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: size.width.0 as u32,
            Height: size.height.0 as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: pixel_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind_flag.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            // This only returns None if the device is lost, which we will recreate later.
            // So it's ok to return None here.
            self.device
                .CreateTexture2D(&texture_desc, None, Some(&mut texture))
                .ok()?;
        }
        let texture = texture.unwrap();

        let texture_list = match kind {
            AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
            AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
            AtlasTextureKind::Subpixel => &mut self.subpixel_textures,
        };
        let index = texture_list.free_list.pop();
        let view = unsafe {
            let mut view = None;
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
                .ok()?;
            [view]
        };
        let atlas_texture = DirectXAtlasTexture {
            id: AtlasTextureId {
                index: index.unwrap_or(texture_list.textures.len()) as u32,
                kind,
            },
            bytes_per_pixel,
            allocator: etagere::BucketedAtlasAllocator::new(device_size_to_etagere(size)),
            texture,
            view,
            live_atlas_keys: 0,
        };
        if let Some(ix) = index {
            texture_list.textures[ix] = Some(atlas_texture);
            texture_list.textures.get_mut(ix).unwrap().as_mut()
        } else {
            texture_list.textures.push(Some(atlas_texture));
            texture_list.textures.last_mut().unwrap().as_mut()
        }
    }

    fn texture(&self, id: AtlasTextureId) -> &DirectXAtlasTexture {
        match id.kind {
            AtlasTextureKind::Monochrome => &self.monochrome_textures[id.index as usize]
                .as_ref()
                .unwrap(),
            AtlasTextureKind::Polychrome => &self.polychrome_textures[id.index as usize]
                .as_ref()
                .unwrap(),
            AtlasTextureKind::Subpixel => {
                &self.subpixel_textures[id.index as usize].as_ref().unwrap()
            }
        }
    }
}

impl DirectXAtlasTexture {
    fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
        let allocation = self.allocator.allocate(device_size_to_etagere(size))?;
        let tile = AtlasTile {
            texture_id: self.id,
            tile_id: allocation.id.into(),
            bounds: Bounds {
                origin: etagere_point_to_device(allocation.rectangle.min),
                size,
            },
            padding: 0,
        };
        self.live_atlas_keys += 1;
        Some(tile)
    }

    fn upload(
        &self,
        device_context: &ID3D11DeviceContext,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) {
        unsafe {
            device_context.UpdateSubresource(
                &self.texture,
                0,
                Some(&D3D11_BOX {
                    left: bounds.left().0 as u32,
                    top: bounds.top().0 as u32,
                    front: 0,
                    right: bounds.right().0 as u32,
                    bottom: bounds.bottom().0 as u32,
                    back: 1,
                }),
                bytes.as_ptr() as _,
                bounds.size.width.to_bytes(self.bytes_per_pixel as u8),
                0,
            );
        }
    }

    fn deallocate(&mut self, tile: AtlasTile) {
        self.allocator.deallocate(tile.tile_id.into());
        self.live_atlas_keys -= 1;
    }

    fn is_unreferenced(&mut self) -> bool {
        self.live_atlas_keys == 0
    }
}

/// Whether opened CEF shared textures may be reused across frames. Read once;
/// `FUTUREBOARD_OSR_NO_HANDLE_CACHE` restores the open-every-frame behaviour so
/// the two can be measured against each other.
fn handle_cache_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_OSR_NO_HANDLE_CACHE").is_none())
}

fn device_size_to_etagere(size: Size<DevicePixels>) -> etagere::Size {
    etagere::Size::new(size.width.into(), size.height.into())
}

fn etagere_point_to_device(value: etagere::Point) -> Point<DevicePixels> {
    Point {
        x: DevicePixels::from(value.x),
        y: DevicePixels::from(value.y),
    }
}
