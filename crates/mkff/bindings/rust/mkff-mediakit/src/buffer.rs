//! Buffers and buffer groups (BeOS `BBuffer` / `BBufferGroup` / `media_header`).
//!
//! A [`BufferGroup`] carves one [`Area`] into equal buffers. A producer requests a buffer, fills
//! it, stamps its [`MediaHeader`] and sends it downstream; when the last consumer drops it, the
//! buffer goes back to its group (`BBuffer::Recycle`) — no copies and no allocation in steady
//! state. Video buffers may instead carry a [`GpuSurface`]: a texture on an accelerant's device
//! that stays in video memory end to end.

use std::any::Any;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::area::Area;
use crate::format::PixelFormat;
use crate::{Error, Result};

/// Per-buffer metadata (`media_header`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaHeader {
    /// Performance time (µs) at which the buffer's first sample should be presented.
    pub start_time: i64,
    /// Media time (µs) of the first sample, for seeking / display.
    pub media_time: i64,
    /// Bytes of payload in use.
    pub size_used: usize,
    /// Audio: frames in the buffer. Video: 1.
    pub frames: u32,
    pub flags: u32,
    /// Video: the picture's layout in the buffer (plane offsets / strides).
    pub video: Option<VideoLayout>,
}

impl MediaHeader {
    /// The first buffer after a seek / discontinuity.
    pub const FLAG_DISCONTINUITY: u32 = 1;
    /// A sync point (keyframe) for encoded data.
    pub const FLAG_KEYFRAME: u32 = 2;
    /// The last buffer of the stream.
    pub const FLAG_END_OF_STREAM: u32 = 4;
}

/// Plane layout of a raw picture in a memory buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoLayout {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Byte offsets of up to three planes.
    pub offsets: [usize; 3],
    pub strides: [usize; 3],
}

/// A picture in GPU memory on an accelerant's device.
#[derive(Clone)]
pub struct GpuSurface {
    /// LUID of the adapter the texture lives on.
    pub adapter_luid: u64,
    /// Shareable handle (`IDXGIResource1::CreateSharedHandle` NT handle on Windows; 0 if the
    /// texture is not shareable).
    pub shared_handle: usize,
    /// Array slice of a texture array (decoder output pools are arrays).
    pub array_index: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Keeps the underlying texture / decoder surface alive while the buffer is in flight.
    pub keep_alive: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for GpuSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuSurface")
            .field("adapter_luid", &format_args!("{:#x}", self.adapter_luid))
            .field("shared_handle", &self.shared_handle)
            .field("array_index", &self.array_index)
            .field("size", &(self.width, self.height))
            .field("format", &self.format)
            .finish()
    }
}

struct GroupShared {
    area: Arc<Area>,
    buffer_size: usize,
    free: Mutex<Vec<u32>>,
    returned: Condvar,
    count: u32,
}

enum Storage {
    /// A slot of a buffer group.
    Slot { group: Arc<GroupShared>, index: u32 },
    /// Standalone heap memory (`Buffer::from_vec`).
    Heap(Vec<u8>),
    /// No CPU payload: a GPU surface.
    Gpu(GpuSurface),
}

/// One media buffer. Dropping it recycles a group buffer.
pub struct Buffer {
    storage: Option<Storage>,
    pub header: MediaHeader,
}

impl Buffer {
    /// A standalone buffer owning `data` (not recycled; for one-off or encoded data).
    pub fn from_vec(data: Vec<u8>) -> Buffer {
        let header = MediaHeader {
            size_used: data.len(),
            ..Default::default()
        };
        Buffer {
            storage: Some(Storage::Heap(data)),
            header,
        }
    }

    /// A buffer whose payload is a GPU surface.
    pub fn from_gpu(surface: GpuSurface) -> Buffer {
        Buffer {
            storage: Some(Storage::Gpu(surface)),
            header: MediaHeader {
                frames: 1,
                ..Default::default()
            },
        }
    }

    /// Capacity in bytes (0 for GPU buffers).
    pub fn capacity(&self) -> usize {
        match &self.storage {
            Some(Storage::Slot { group, .. }) => group.buffer_size,
            Some(Storage::Heap(v)) => v.len(),
            _ => 0,
        }
    }

    /// The whole payload area (`BBuffer::Data`).
    pub fn data(&self) -> &[u8] {
        match &self.storage {
            Some(Storage::Slot { group, index }) => {
                // SAFETY: slot `index` is owned exclusively by this buffer until it is dropped.
                unsafe {
                    group
                        .area
                        .range(*index as usize * group.buffer_size, group.buffer_size)
                }
            }
            Some(Storage::Heap(v)) => v,
            _ => &[],
        }
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        match &mut self.storage {
            Some(Storage::Slot { group, index }) => {
                // SAFETY: as above, and `&mut self` makes the access exclusive.
                unsafe {
                    group
                        .area
                        .range_mut(*index as usize * group.buffer_size, group.buffer_size)
                }
            }
            Some(Storage::Heap(v)) => v,
            _ => &mut [],
        }
    }

    /// The bytes in use (`data()[..header.size_used]`).
    pub fn payload(&self) -> &[u8] {
        let d = self.data();
        &d[..self.header.size_used.min(d.len())]
    }

    /// Payload as f32 samples (audio buffers in `F32` / `F32Planar` formats).
    pub fn as_f32(&self) -> impl Iterator<Item = f32> + '_ {
        self.payload()
            .chunks_exact(4)
            .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Write f32 samples at the start of the buffer and set `size_used`. Fails if they don't fit.
    pub fn write_f32(&mut self, samples: &[f32]) -> Result<()> {
        let n = samples.len() * 4;
        let d = self.data_mut();
        if n > d.len() {
            return Err(Error::InvalidArgument(format!(
                "{n} bytes into a {}-byte buffer",
                d.len()
            )));
        }
        for (dst, s) in d.chunks_exact_mut(4).zip(samples) {
            dst.copy_from_slice(&s.to_ne_bytes());
        }
        self.header.size_used = n;
        Ok(())
    }

    pub fn gpu_surface(&self) -> Option<&GpuSurface> {
        match &self.storage {
            Some(Storage::Gpu(s)) => Some(s),
            _ => None,
        }
    }

    /// Id of the area holding the payload (0 for heap / GPU buffers).
    pub fn area_id(&self) -> u32 {
        match &self.storage {
            Some(Storage::Slot { group, .. }) => group.area.id(),
            _ => 0,
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let Some(Storage::Slot { group, index }) = self.storage.take() {
            group
                .free
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(index);
            group.returned.notify_one();
        }
    }
}

impl std::fmt::Debug for Buffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.storage {
            Some(Storage::Slot { index, .. }) => format!("group slot {index}"),
            Some(Storage::Heap(_)) => "heap".into(),
            Some(Storage::Gpu(s)) => format!("{s:?}"),
            None => "released".into(),
        };
        f.debug_struct("Buffer")
            .field("storage", &kind)
            .field("header", &self.header)
            .finish()
    }
}

/// A pool of equal-sized buffers in one area (`BBufferGroup`).
#[derive(Clone)]
pub struct BufferGroup {
    shared: Arc<GroupShared>,
}

impl BufferGroup {
    /// `count` buffers of `size` bytes in a private area.
    pub fn new(size: usize, count: u32) -> Result<BufferGroup> {
        Self::with_area(None, size, count)
    }

    /// Like [`Self::new`] in a named area another process can map ([`Area::clone_named`]).
    pub fn named(name: &str, size: usize, count: u32) -> Result<BufferGroup> {
        Self::with_area(Some(name), size, count)
    }

    fn with_area(name: Option<&str>, size: usize, count: u32) -> Result<BufferGroup> {
        if size == 0 || count == 0 {
            return Err(Error::InvalidArgument("empty buffer group".into()));
        }
        // keep each buffer 64-byte aligned for SIMD consumers
        let size = size.div_ceil(64) * 64;
        let total = size
            .checked_mul(count as usize)
            .ok_or_else(|| Error::InvalidArgument("buffer group too large".into()))?;
        let area = match name {
            Some(n) => Area::named(n, total)?,
            None => Area::new(total)?,
        };
        Ok(BufferGroup {
            shared: Arc::new(GroupShared {
                area: Arc::new(area),
                buffer_size: size,
                free: Mutex::new((0..count).rev().collect()),
                returned: Condvar::new(),
                count,
            }),
        })
    }

    /// Get a free buffer of at least `size` bytes, waiting up to `timeout` for one to be
    /// recycled (`RequestBuffer`; `None` waits forever).
    pub fn request_buffer(&self, size: usize, timeout: Option<Duration>) -> Result<Buffer> {
        if size > self.shared.buffer_size {
            return Err(Error::InvalidArgument(format!(
                "{size} bytes from a group of {}-byte buffers",
                self.shared.buffer_size
            )));
        }
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut free = self.shared.free.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(index) = free.pop() {
                return Ok(Buffer {
                    storage: Some(Storage::Slot {
                        group: Arc::clone(&self.shared),
                        index,
                    }),
                    header: MediaHeader::default(),
                });
            }
            match deadline {
                None => {
                    free = self
                        .shared
                        .returned
                        .wait(free)
                        .unwrap_or_else(|e| e.into_inner())
                }
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(Error::TimedOut);
                    }
                    free = self
                        .shared
                        .returned
                        .wait_timeout(free, d - now)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
    }

    pub fn buffer_size(&self) -> usize {
        self.shared.buffer_size
    }

    /// Buffers in the group (`CountBuffers`).
    pub fn count_buffers(&self) -> u32 {
        self.shared.count
    }

    /// Buffers currently free.
    pub fn free_buffers(&self) -> usize {
        self.shared
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn area(&self) -> &Area {
        &self.shared.area
    }

    /// Wait until every buffer is back (before tearing down a connection).
    pub fn wait_for_buffers(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut free = self.shared.free.lock().unwrap_or_else(|e| e.into_inner());
        while free.len() < self.shared.count as usize {
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::TimedOut);
            }
            free = self
                .shared
                .returned
                .wait_timeout(free, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(())
    }

    /// A weak handle (does not keep the area alive).
    pub fn downgrade(&self) -> WeakBufferGroup {
        WeakBufferGroup(Arc::downgrade(&self.shared))
    }
}

/// Weak reference to a [`BufferGroup`].
#[derive(Clone)]
pub struct WeakBufferGroup(Weak<GroupShared>);

impl WeakBufferGroup {
    pub fn upgrade(&self) -> Option<BufferGroup> {
        self.0.upgrade().map(|shared| BufferGroup { shared })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle() {
        let g = BufferGroup::new(1000, 2).unwrap();
        assert_eq!(g.buffer_size(), 1024);
        let mut a = g.request_buffer(512, None).unwrap();
        let b = g.request_buffer(512, None).unwrap();
        a.write_f32(&[1.0, 2.0]).unwrap();
        assert_eq!(a.as_f32().collect::<Vec<_>>(), vec![1.0, 2.0]);
        assert_eq!(
            g.request_buffer(10, Some(Duration::from_millis(10)))
                .unwrap_err(),
            Error::TimedOut
        );
        let g2 = g.clone();
        let t = std::thread::spawn(move || {
            g2.request_buffer(10, Some(Duration::from_secs(5)))
                .map(|_| ())
        });
        std::thread::sleep(Duration::from_millis(20));
        drop(b);
        t.join().unwrap().unwrap();
        drop(a);
        g.wait_for_buffers(Duration::from_secs(1)).unwrap();
        assert_eq!(g.free_buffers(), 2);
    }

    #[test]
    fn slots_are_disjoint() {
        let g = BufferGroup::new(64, 4).unwrap();
        let mut bufs: Vec<Buffer> = (0..4)
            .map(|_| g.request_buffer(64, None).unwrap())
            .collect();
        for (i, b) in bufs.iter_mut().enumerate() {
            b.data_mut().fill(i as u8);
        }
        for (i, b) in bufs.iter().enumerate() {
            assert!(b.data().iter().all(|&x| x == i as u8));
        }
    }
}
