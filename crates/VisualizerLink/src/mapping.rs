//! A named shared-memory region: created by the writer (Studio), opened by
//! readers (the visualizer app). Windows: a pagefile-backed file mapping in
//! the session namespace, gone when the last process unmaps it. POSIX:
//! `shm_open`, unlinked by the writer when it goes away.

pub(crate) struct Mapping {
    imp: imp::Platform,
    size: usize,
}

// The region is shared memory; who writes what is the protocol's contract.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Creates `name`, zero-filled, or maps the existing one. The flag says
    /// whether it already existed: the caller decides whether to take it
    /// over.
    pub(crate) fn create_or_open(name: &str, size: usize) -> std::io::Result<(Self, bool)> {
        let (imp, existed) = imp::Platform::create_or_open(name, size)?;
        Ok((Self { imp, size }, existed))
    }

    pub(crate) fn open(name: &str, size: usize) -> std::io::Result<Self> {
        Ok(Self {
            imp: imp::Platform::open(name, size)?,
            size,
        })
    }

    pub(crate) fn ptr(&self) -> *mut u8 {
        self.imp.ptr().cast()
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows::core::HSTRING;

    pub(crate) struct Platform {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    impl Platform {
        pub(crate) fn create_or_open(name: &str, size: usize) -> std::io::Result<(Self, bool)> {
            let wide = HSTRING::from(name);
            // SAFETY: INVALID_HANDLE_VALUE backs the mapping with the pagefile.
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    ((size as u64) >> 32) as u32,
                    size as u32,
                    &wide,
                )
            }
            .map_err(std::io::Error::other)?;
            let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            Ok((Self::map(handle, size)?, existed))
        }

        pub(crate) fn open(name: &str, size: usize) -> std::io::Result<Self> {
            let wide = HSTRING::from(name);
            let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, &wide) }
                .map_err(std::io::Error::other)?;
            Self::map(handle, size)
        }

        fn map(handle: HANDLE, size: usize) -> std::io::Result<Self> {
            // SAFETY: a valid mapping handle; the view is unmapped on drop.
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) };
            if view.Value.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe {
                    let _ = CloseHandle(handle);
                }
                return Err(error);
            }
            Ok(Self { handle, view })
        }

        pub(crate) fn ptr(&self) -> *mut core::ffi::c_void {
            self.view.Value
        }
    }

    impl Drop for Platform {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(self.view);
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;

    pub(crate) struct Platform {
        ptr: *mut core::ffi::c_void,
        size: usize,
        name: CString,
        owner: bool,
    }

    fn last_error(call: &str) -> std::io::Error {
        let error = std::io::Error::last_os_error();
        std::io::Error::new(error.kind(), format!("{call}: {error}"))
    }

    fn c_name(name: &str) -> std::io::Result<CString> {
        CString::new(name)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid name"))
    }

    impl Platform {
        pub(crate) fn create_or_open(name: &str, size: usize) -> std::io::Result<(Self, bool)> {
            let name = c_name(name)?;
            let fd = unsafe {
                libc::shm_open(
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600 as libc::mode_t,
                )
            };
            if fd < 0 {
                if std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(last_error("shm_open (create)"));
                }
                // Left by a Studio that is gone or still running: map it,
                // owned, so this one unlinks it in turn.
                let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
                if fd < 0 {
                    return Err(last_error("shm_open (existing)"));
                }
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstat(fd, &mut stat) } != 0 || (stat.st_size as usize) < size {
                    unsafe {
                        libc::close(fd);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "an existing visualizer link has another size",
                    ));
                }
                return Ok((Self::map(fd, size, name, true)?, true));
            }
            if unsafe { libc::ftruncate(fd, size as libc::off_t) } != 0 {
                let error = last_error("ftruncate");
                unsafe {
                    libc::close(fd);
                    libc::shm_unlink(name.as_ptr());
                }
                return Err(error);
            }
            Ok((Self::map(fd, size, name, true)?, false))
        }

        pub(crate) fn open(name: &str, size: usize) -> std::io::Result<Self> {
            let name = c_name(name)?;
            let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
            if fd < 0 {
                return Err(last_error("shm_open (open)"));
            }
            Self::map(fd, size, name, false)
        }

        fn map(fd: libc::c_int, size: usize, name: CString, owner: bool) -> std::io::Result<Self> {
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            let error = last_error("mmap");
            unsafe {
                libc::close(fd);
            }
            if ptr == libc::MAP_FAILED {
                if owner {
                    unsafe {
                        libc::shm_unlink(name.as_ptr());
                    }
                }
                return Err(error);
            }
            Ok(Self {
                ptr,
                size,
                name,
                owner,
            })
        }

        pub(crate) fn ptr(&self) -> *mut core::ffi::c_void {
            self.ptr
        }
    }

    impl Drop for Platform {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr, self.size);
                if self.owner {
                    libc::shm_unlink(self.name.as_ptr());
                }
            }
        }
    }
}
