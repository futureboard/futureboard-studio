//! Areas: page-aligned memory that can be shared between processes (BeOS `create_area` /
//! `clone_area`).
//!
//! On Windows an area is a pagefile-backed file mapping (`CreateFileMappingW`), optionally named
//! (`Local\…`) so another process can [`Area::clone_named`] it and see the same pages — the
//! mechanism BeOS used to hand media buffers between applications and the media server without
//! copying. Elsewhere an area is ordinary heap memory and cannot be cloned by name.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::{Error, Result};

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// A block of memory that buffers are carved from.
pub struct Area {
    id: u32,
    name: Option<String>,
    ptr: *mut u8,
    len: usize,
    backing: Backing,
}

// SAFETY: the area owns its mapping; concurrent access to disjoint byte ranges is coordinated by
// `BufferGroup` (one `Buffer` per range at a time), and whole-area access goes through `&mut`.
unsafe impl Send for Area {}
unsafe impl Sync for Area {}

enum Backing {
    #[allow(dead_code)]
    Heap(Box<[u8]>),
    #[cfg(windows)]
    Mapping {
        handle: windows::Win32::Foundation::HANDLE,
        view: windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    },
}

impl Area {
    /// A private area of at least `size` bytes (rounded up to 4 KiB).
    pub fn new(size: usize) -> Result<Area> {
        Self::create(None, size)
    }

    /// An area other processes can open with [`Area::clone_named`] (Windows only; elsewhere the
    /// name is kept for display and the memory is private).
    pub fn named(name: &str, size: usize) -> Result<Area> {
        Self::create(Some(name), size)
    }

    fn create(name: Option<&str>, size: usize) -> Result<Area> {
        if size == 0 {
            return Err(Error::InvalidArgument("area size 0".into()));
        }
        let len = size.div_ceil(4096) * 4096;
        #[cfg(windows)]
        {
            platform::create(name, len)
        }
        #[cfg(not(windows))]
        {
            let mut mem = vec![0u8; len].into_boxed_slice();
            let ptr = mem.as_mut_ptr();
            Ok(Area {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                name: name.map(str::to_owned),
                ptr,
                len,
                backing: Backing::Heap(mem),
            })
        }
    }

    /// Open an area another process (or this one) created with [`Area::named`].
    pub fn clone_named(name: &str) -> Result<Area> {
        #[cfg(windows)]
        {
            platform::open(name)
        }
        #[cfg(not(windows))]
        {
            Err(Error::Unsupported(format!(
                "clone_area({name}): named areas need Windows"
            )))
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the area lives in a shareable OS mapping.
    pub fn is_shared(&self) -> bool {
        !matches!(self.backing, Backing::Heap(_))
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `len` bytes for the area's lifetime.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Raw view of `range`; the caller guarantees no other live reference overlaps it.
    ///
    /// # Safety
    /// `offset + len <= self.len()`, and the range must not be accessed through any other
    /// reference while the returned slice lives.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn range_mut(&self, offset: usize, len: usize) -> &mut [u8] {
        debug_assert!(offset + len <= self.len);
        std::slice::from_raw_parts_mut(self.ptr.add(offset), len)
    }

    /// # Safety
    /// As [`Self::range_mut`], for shared reads.
    pub(crate) unsafe fn range(&self, offset: usize, len: usize) -> &[u8] {
        debug_assert!(offset + len <= self.len);
        std::slice::from_raw_parts(self.ptr.add(offset), len)
    }
}

impl Drop for Area {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Backing::Mapping { handle, view } = &self.backing {
            platform::release(*handle, *view);
        }
    }
}

impl std::fmt::Debug for Area {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Area")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("len", &self.len)
            .field("shared", &self.is_shared())
            .finish()
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
        FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };

    fn kernel_name(name: &str) -> HSTRING {
        if name.contains('\\') {
            HSTRING::from(name)
        } else {
            HSTRING::from(format!("Local\\mkff.area.{name}"))
        }
    }

    pub(super) fn create(name: Option<&str>, len: usize) -> Result<Area> {
        let kname = name.map(kernel_name);
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (len as u64 >> 32) as u32,
                len as u32,
                kname.as_ref().map_or(windows::core::PCWSTR::null(), |n| {
                    windows::core::PCWSTR(n.as_ptr())
                }),
            )
            .map_err(|e| Error::Os(format!("create_area: {e}")))?;
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len);
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return Err(Error::Os("create_area: MapViewOfFile failed".into()));
            }
            Ok(Area {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                name: name.map(str::to_owned),
                ptr: view.Value as *mut u8,
                len,
                backing: Backing::Mapping { handle, view },
            })
        }
    }

    pub(super) fn open(name: &str) -> Result<Area> {
        let kname = kernel_name(name);
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &kname)
                .map_err(|e| Error::Os(format!("clone_area({name}): {e}")))?;
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0);
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return Err(Error::Os(format!(
                    "clone_area({name}): MapViewOfFile failed"
                )));
            }
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let n = VirtualQuery(
                Some(view.Value),
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            let len = if n == 0 { 0 } else { info.RegionSize };
            Ok(Area {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                name: Some(name.to_owned()),
                ptr: view.Value as *mut u8,
                len,
                backing: Backing::Mapping { handle, view },
            })
        }
    }

    pub(super) fn release(handle: HANDLE, view: MEMORY_MAPPED_VIEW_ADDRESS) {
        unsafe {
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_write() {
        let mut a = Area::new(10_000).unwrap();
        assert_eq!(a.len(), 12_288);
        a.as_mut_slice()[123] = 7;
        assert_eq!(a.as_slice()[123], 7);
    }

    #[cfg(windows)]
    #[test]
    fn named_area_is_shared() {
        let name = format!("test.{}", std::process::id());
        let mut a = Area::named(&name, 4096).unwrap();
        assert!(a.is_shared());
        a.as_mut_slice()[..4].copy_from_slice(b"BeOS");
        let b = Area::clone_named(&name).unwrap();
        assert_eq!(&b.as_slice()[..4], b"BeOS");
        assert!(b.len() >= 4096);
    }
}
