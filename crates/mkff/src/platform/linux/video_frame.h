#ifndef MKFF_LINUX_VIDEO_FRAME_H
#define MKFF_LINUX_VIDEO_FRAME_H

#include <va/va.h>
#include <stdatomic.h>

#include "mkff/cpu_planes.h"
#include "mkff/video_frame.h"
#include "src/common/mkff_common.h"

typedef struct DecoderShared DecoderShared;

typedef struct LinuxVideoFrame {
    MKFF_HandleCommon common; /* must be first: core dispatches through this */

    DecoderShared *shared; /* one reference held for the lifetime of this frame */
    uint32_t       pool_index;
    VASurfaceID    surface;

    atomic_int refcount;

    MKFF_VideoFrameInfo info;

    /* Set while CPU planes are mapped via vaDeriveImage + vaMapBuffer. */
    int     cpu_mapped;
    VAImage cpu_image;
} LinuxVideoFrame;

/* Allocates a frame with refcount 1, taking one reference on `shared`. */
LinuxVideoFrame *linux_video_frame_create(DecoderShared *shared,
                                           uint32_t pool_index,
                                           VASurfaceID surface,
                                           uint32_t width,
                                           uint32_t height,
                                           int64_t pts,
                                           int64_t dts,
                                           int is_key_frame,
                                           MKFF_PixelFormat format);

/* These match MKFF_PlatformAPI's video_frame_* signatures exactly and
 * are wired directly into the exported vtable. */
MKFF_VideoFrame *linux_video_frame_retain(MKFF_VideoFrame *frame);
void             linux_video_frame_release(MKFF_VideoFrame *frame);
MKFF_Result      linux_video_frame_get_info(const MKFF_VideoFrame *frame, MKFF_VideoFrameInfo *out_info);
MKFF_Result      linux_video_frame_export_dmabuf(const MKFF_VideoFrame *frame, MKFF_LinuxDmaBufDesc *out_desc);
MKFF_Result      linux_video_frame_map_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *out_planes);
void             linux_video_frame_unmap_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *planes);

#endif /* MKFF_LINUX_VIDEO_FRAME_H */
