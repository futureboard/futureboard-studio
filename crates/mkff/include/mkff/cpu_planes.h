#ifndef MKFF_CPU_PLANES_H
#define MKFF_CPU_PLANES_H

#include <stddef.h>
#include <stdint.h>

#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"
#include "mkff/video_frame.h"

MKFF_BEGIN_DECLS

#define MKFF_CPU_PLANES_MAX 4

typedef struct MKFF_CpuPlaneDesc {
    MKFF_STRUCT_HEADER;

    MKFF_PixelFormat format;
    uint32_t width;
    uint32_t height;
    uint32_t plane_count;

    /* Pointers valid until mkff_video_frame_unmap_cpu_planes(). */
    const uint8_t *data[MKFF_CPU_PLANES_MAX];
    uint32_t stride[MKFF_CPU_PLANES_MAX];
    uint32_t height_lines[MKFF_CPU_PLANES_MAX];
} MKFF_CpuPlaneDesc;

/* Maps a frame's CPU planes for read-only access (NV12 / P010).
 *
 * Software frames expose decoder-owned buffers directly. Hardware frames
 * succeed when the platform implements readback (Windows: D3D11 staging
 * copy; macOS: CVPixelBuffer lock; Linux: vaDeriveImage + vaMapBuffer
 * when the surface is CPU-mappable). Otherwise returns
 * MKFF_RESULT_ERROR_NOT_SUPPORTED. Zero-copy GPU export paths are
 * unchanged and remain preferred when the consumer can use them.
 *
 * Pointers in out_planes are valid until mkff_video_frame_unmap_cpu_planes(). */
MKFF_API MKFF_Result mkff_video_frame_map_cpu_planes(const MKFF_VideoFrame *frame,
                                                      MKFF_CpuPlaneDesc *out_planes);
MKFF_API void         mkff_video_frame_unmap_cpu_planes(const MKFF_VideoFrame *frame,
                                                        MKFF_CpuPlaneDesc *planes);

MKFF_END_DECLS

#endif /* MKFF_CPU_PLANES_H */
