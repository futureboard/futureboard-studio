#ifndef MKFF_MACOS_VIDEO_FRAME_H
#define MKFF_MACOS_VIDEO_FRAME_H

#include <CoreVideo/CoreVideo.h>
#include <stdatomic.h>

#include "mkff/cpu_planes.h"
#include "mkff/macos/iosurface.h"
#include "mkff/video_frame.h"
#include "src/common/mkff_common.h"

typedef struct DecoderShared DecoderShared;

typedef struct MacosVideoFrame {
    MKFF_HandleCommon common; /* must be first: core dispatches through this */

    DecoderShared    *shared; /* one reference held for the lifetime of this frame */
    CVPixelBufferRef   pixel_buffer; /* CVPixelBufferRetain'd once for this frame */

    atomic_int refcount;

    MKFF_VideoFrameInfo info;

    int cpu_planes_mapped; /* non-zero while CVPixelBuffer base addresses are locked */
} MacosVideoFrame;

/* Takes ownership of one CVPixelBufferRetain reference on `pixel_buffer`
 * (i.e. the caller must already have retained it for this call, or
 * pass a +1 reference it is giving up). */
MacosVideoFrame *macos_video_frame_create(DecoderShared *shared,
                                           CVPixelBufferRef pixel_buffer,
                                           uint32_t width,
                                           uint32_t height,
                                           MKFF_PixelFormat format,
                                           int64_t pts,
                                           int64_t dts,
                                           int is_key_frame);

/* These match MKFF_PlatformAPI's video_frame_* signatures exactly and
 * are wired directly into the exported vtable. */
MKFF_VideoFrame *macos_video_frame_retain(MKFF_VideoFrame *frame);
void             macos_video_frame_release(MKFF_VideoFrame *frame);
MKFF_Result      macos_video_frame_get_info(const MKFF_VideoFrame *frame, MKFF_VideoFrameInfo *out_info);
MKFF_Result      macos_video_frame_export_iosurface(const MKFF_VideoFrame *frame, MKFF_MacosIOSurfaceDesc *out_desc);
MKFF_Result      macos_video_frame_map_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *out_planes);
void             macos_video_frame_unmap_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *planes);

#endif /* MKFF_MACOS_VIDEO_FRAME_H */
