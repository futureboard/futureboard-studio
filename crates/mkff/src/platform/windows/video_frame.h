#ifndef MKFF_WINDOWS_VIDEO_FRAME_H
#define MKFF_WINDOWS_VIDEO_FRAME_H

#include <stdatomic.h>

#include "mkff/cpu_planes.h"
#include "mkff/video_frame.h"
#include "mkff/windows/d3d11.h"
#include "src/common/mkff_common.h"
#include "win_common.h"

typedef struct DecoderShared DecoderShared;

typedef struct WindowsVideoFrame {
    MKFF_HandleCommon common; /* must be first: core dispatches through this */

    DecoderShared *shared; /* one reference held for the lifetime of this frame */
    uint32_t       array_slice;

    atomic_int refcount;

    MKFF_VideoFrameInfo info;

    /* Owned while CPU planes are mapped; released on unmap / frame release. */
    ID3D11Texture2D *staging;
} WindowsVideoFrame;

WindowsVideoFrame *windows_video_frame_create(DecoderShared *shared,
                                               uint32_t array_slice,
                                               uint32_t width,
                                               uint32_t height,
                                               MKFF_PixelFormat format,
                                               int64_t pts,
                                               int64_t dts,
                                               int is_key_frame);

/* These match MKFF_PlatformAPI's video_frame_* signatures exactly and
 * are wired directly into the exported vtable. */
MKFF_VideoFrame *windows_video_frame_retain(MKFF_VideoFrame *frame);
void             windows_video_frame_release(MKFF_VideoFrame *frame);
MKFF_Result      windows_video_frame_get_info(const MKFF_VideoFrame *frame, MKFF_VideoFrameInfo *out_info);
MKFF_Result      windows_video_frame_export_d3d11_texture(const MKFF_VideoFrame *frame, MKFF_WindowsD3D11TextureDesc *out_desc);
MKFF_Result      windows_video_frame_map_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *out_planes);
void             windows_video_frame_unmap_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *planes);

#endif /* MKFF_WINDOWS_VIDEO_FRAME_H */
