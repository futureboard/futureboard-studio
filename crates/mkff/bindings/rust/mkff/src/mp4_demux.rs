use std::ffi::CString;
use std::path::Path;
use std::ptr;

use crate::error::{check, Result};

/// Owns one progressive MP4/MOV demuxer (`MKFF_Mp4Demux`).
///
/// Emits Annex-B access units for the first H.264 (`avc1`) or HEVC
/// (`hvc1`/`hev1`) video track. Not `Send`/`Sync`: the C API does not
/// document concurrent use of the same demuxer.
pub struct Mp4Demux {
    ptr: *mut mkff_sys::MKFF_Mp4Demux,
}

/// First video track summary from [`Mp4Demux::video_track`].
#[derive(Debug, Clone, Copy)]
pub struct Mp4VideoTrackInfo {
    pub codec: mkff_sys::MKFF_VideoCodec,
    pub width: u32,
    pub height: u32,
    pub timescale: u32,
    pub duration: u64,
    pub sample_count: u32,
}

impl Mp4VideoTrackInfo {
    /// Duration in seconds (`duration / timescale`), or `None` if timescale is 0.
    pub fn duration_secs(&self) -> Option<f64> {
        if self.timescale == 0 {
            None
        } else {
            Some(self.duration as f64 / self.timescale as f64)
        }
    }
}

/// One Annex-B access unit. `data` is borrowed from the demuxer and is
/// invalidated by the next [`Mp4Demux::read_access_unit`],
/// [`Mp4Demux::seek_sample`], or drop.
#[derive(Debug, Clone, Copy)]
pub struct Mp4AccessUnit<'a> {
    pub data: &'a [u8],
    pub pts: i64,
    pub dts: i64,
    pub sync: bool,
    pub sample_index: u32,
}

/// Outcome of [`Mp4Demux::read_access_unit`].
pub enum ReadAuOutcome<'a> {
    Au(Mp4AccessUnit<'a>),
    EndOfStream,
}

impl Mp4Demux {
    /// Opens a progressive MP4/MOV file (`moov` + `mdat`). Copies file bytes.
    ///
    /// Prefer [`open_memory`](Self::open_memory) with `std::fs::read` when the
    /// path may contain non-UTF-8 / non-ANSI characters (Windows `fopen`).
    pub fn open_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let path_str = path.to_str().ok_or(crate::error::MkffError(
            mkff_sys::MKFF_Result::MKFF_RESULT_ERROR_INVALID_ARGUMENT,
        ))?;
        let c_path = CString::new(path_str).map_err(|_| {
            crate::error::MkffError(mkff_sys::MKFF_Result::MKFF_RESULT_ERROR_INVALID_ARGUMENT)
        })?;
        let mut ptr = ptr::null_mut();
        check(unsafe { mkff_sys::mkff_mp4_demux_open_path(c_path.as_ptr(), &mut ptr) })?;
        Ok(Mp4Demux { ptr })
    }

    /// Opens from a memory buffer. Copies `data` (caller may free immediately).
    pub fn open_memory(data: &[u8]) -> Result<Self> {
        let mut ptr = ptr::null_mut();
        check(unsafe { mkff_sys::mkff_mp4_demux_open_memory(data.as_ptr(), data.len(), &mut ptr) })?;
        Ok(Mp4Demux { ptr })
    }

    pub fn video_track(&self) -> Result<Mp4VideoTrackInfo> {
        let mut raw: mkff_sys::MKFF_Mp4VideoTrackInfo = unsafe { std::mem::zeroed() };
        raw.struct_size = std::mem::size_of::<mkff_sys::MKFF_Mp4VideoTrackInfo>() as u32;
        raw.abi_version = mkff_sys::MKFF_ABI_VERSION;
        check(unsafe { mkff_sys::mkff_mp4_demux_get_video_track(self.ptr, &mut raw) })?;
        Ok(Mp4VideoTrackInfo {
            codec: raw.codec,
            width: raw.width,
            height: raw.height,
            timescale: raw.timescale,
            duration: raw.duration,
            sample_count: raw.sample_count,
        })
    }

    /// Reads the next sample as an Annex-B AU. Parameter sets from
    /// `avcC`/`hvcC` are prepended on the first sample and on sync samples.
    pub fn read_access_unit(&mut self) -> Result<ReadAuOutcome<'_>> {
        let mut raw: mkff_sys::MKFF_Mp4AccessUnit = unsafe { std::mem::zeroed() };
        raw.struct_size = std::mem::size_of::<mkff_sys::MKFF_Mp4AccessUnit>() as u32;
        raw.abi_version = mkff_sys::MKFF_ABI_VERSION;
        let result = unsafe { mkff_sys::mkff_mp4_demux_read_access_unit(self.ptr, &mut raw) };
        match result {
            mkff_sys::MKFF_Result::MKFF_RESULT_OK => {
                let data = if raw.data.is_null() || raw.size == 0 {
                    &[][..]
                } else {
                    unsafe { std::slice::from_raw_parts(raw.data, raw.size) }
                };
                Ok(ReadAuOutcome::Au(Mp4AccessUnit {
                    data,
                    pts: raw.pts,
                    dts: raw.dts,
                    sync: raw.sync != 0,
                    sample_index: raw.sample_index,
                }))
            }
            mkff_sys::MKFF_Result::MKFF_RESULT_END_OF_STREAM => Ok(ReadAuOutcome::EndOfStream),
            other => Err(crate::error::MkffError(other)),
        }
    }

    /// Seeks so the next [`read_access_unit`](Self::read_access_unit) returns
    /// sample `sample_index` (0-based).
    pub fn seek_sample(&mut self, sample_index: u32) -> Result<()> {
        check(unsafe { mkff_sys::mkff_mp4_demux_seek_sample(self.ptr, sample_index) })
    }
}

impl Drop for Mp4Demux {
    fn drop(&mut self) {
        unsafe { mkff_sys::mkff_mp4_demux_destroy(self.ptr) };
    }
}
