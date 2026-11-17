use std::marker::PhantomData;
use std::ptr;

use crate::context::Context;
use crate::error::{check, Result};
use crate::frame::VideoFrame;

/// Owns one `MKFF_VideoDecoder`. Borrows its `Context` for its whole
/// lifetime purely as a conservative ownership convention (unlike
/// `VideoFrame`, which the C API explicitly allows to outlive its
/// `Context`) — it costs nothing and keeps "decoder belongs to a
/// context" obvious at the type level.
///
/// Not `Send`/`Sync`: `mkff_video_decoder_submit/receive/flush` are not
/// documented as safe to call concurrently on the same decoder.
pub struct VideoDecoder<'ctx> {
    ptr: *mut mkff_sys::MKFF_VideoDecoder,
    _ctx: PhantomData<&'ctx Context>,
}

pub enum ReceiveOutcome {
    Frame(VideoFrame),
    NotReady,
    EndOfStream,
}

#[derive(Debug, Clone, Copy)]
pub struct DecoderInfo {
    pub width: u32,
    pub height: u32,
    pub profile: mkff_sys::MKFF_VideoProfile,
    pub entrypoint: mkff_sys::MKFF_VideoEntrypoint,
    pub output_format: mkff_sys::MKFF_PixelFormat,
    pub surface_pool_size: u32,
    pub surface_pool_capacity: u32,
    pub backend: mkff_sys::MKFF_VideoBackend,
    pub bit_depth: u32,
    pub chroma_format_idc: u32,
    pub hardware: bool,
}

impl<'ctx> VideoDecoder<'ctx> {
    pub(crate) fn create(
        context: &'ctx Context,
        codec: mkff_sys::MKFF_VideoCodec,
        max_surfaces: u32,
        backend: mkff_sys::MKFF_VideoBackend,
    ) -> Result<Self> {
        let desc = mkff_sys::MKFF_VideoDecoderDesc {
            struct_size: std::mem::size_of::<mkff_sys::MKFF_VideoDecoderDesc>() as u32,
            abi_version: mkff_sys::MKFF_ABI_VERSION,
            reserved: [0; 4],
            codec,
            max_surfaces,
            width_hint: 0,
            height_hint: 0,
            backend,
        };

        let mut ptr = ptr::null_mut();
        check(unsafe { mkff_sys::mkff_video_decoder_create(context.as_raw(), &desc, &mut ptr) })?;
        Ok(VideoDecoder { ptr, _ctx: PhantomData })
    }

    /// Submits one Annex-B access unit (one or more NAL units for a
    /// single coded picture).
    pub fn submit(&mut self, annex_b: &[u8], pts: Option<i64>, dts: Option<i64>) -> Result<()> {
        let pts = pts.unwrap_or(mkff_sys::MKFF_TIMESTAMP_NONE);
        let dts = dts.unwrap_or(mkff_sys::MKFF_TIMESTAMP_NONE);
        check(unsafe { mkff_sys::mkff_video_decoder_submit(self.ptr, annex_b.as_ptr(), annex_b.len(), pts, dts) })
    }

    pub fn receive(&mut self) -> Result<ReceiveOutcome> {
        let mut frame_ptr = ptr::null_mut();
        let result = unsafe { mkff_sys::mkff_video_decoder_receive(self.ptr, &mut frame_ptr) };
        match result {
            mkff_sys::MKFF_Result::MKFF_RESULT_OK => Ok(ReceiveOutcome::Frame(unsafe { VideoFrame::from_raw_owned(frame_ptr) })),
            mkff_sys::MKFF_Result::MKFF_RESULT_NOT_READY => Ok(ReceiveOutcome::NotReady),
            mkff_sys::MKFF_Result::MKFF_RESULT_END_OF_STREAM => Ok(ReceiveOutcome::EndOfStream),
            other => Err(crate::error::MkffError(other)),
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        check(unsafe { mkff_sys::mkff_video_decoder_flush(self.ptr) })
    }

    pub fn info(&self) -> Result<DecoderInfo> {
        let mut raw: mkff_sys::MKFF_VideoDecoderInfo = unsafe { std::mem::zeroed() };
        raw.struct_size = std::mem::size_of::<mkff_sys::MKFF_VideoDecoderInfo>() as u32;
        raw.abi_version = mkff_sys::MKFF_ABI_VERSION;
        check(unsafe { mkff_sys::mkff_video_decoder_get_info(self.ptr, &mut raw) })?;
        Ok(DecoderInfo {
            width: raw.width,
            height: raw.height,
            profile: raw.profile,
            entrypoint: raw.entrypoint,
            output_format: raw.output_format,
            surface_pool_size: raw.surface_pool_size,
            surface_pool_capacity: raw.surface_pool_capacity,
            backend: raw.backend,
            bit_depth: raw.bit_depth,
            chroma_format_idc: raw.chroma_format_idc,
            hardware: raw.hardware != 0,
        })
    }
}

impl Drop for VideoDecoder<'_> {
    fn drop(&mut self) {
        unsafe { mkff_sys::mkff_video_decoder_destroy(self.ptr) };
    }
}
