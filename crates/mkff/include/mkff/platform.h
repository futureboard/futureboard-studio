#ifndef MKFF_PLATFORM_H
#define MKFF_PLATFORM_H

/*
 * Contract between libmkff (the portable core) and platform modules such
 * as libmkff_platform_linux.so. Application code never includes this
 * header directly or calls these entry points: it goes through the
 * public mkff_* / mkff_linux_* API in libmkff, which loads the platform
 * module dynamically and dispatches through this vtable.
 *
 * A platform module exports exactly one public symbol:
 *
 *   const MKFF_PlatformAPI *mkff_platform_get_api(uint32_t requested_abi_version);
 *
 * It must return NULL if it cannot serve `requested_abi_version` (e.g.
 * the module is older than the core, or a future incompatible core asks
 * for a version the module predates). The core treats a NULL return, a
 * missing symbol, or a dlopen() failure identically: platform-backed
 * functionality is unavailable and calls into it fail with
 * MKFF_RESULT_ERROR_PLATFORM_LOAD.
 */

#include <stddef.h>
#include <stdint.h>

#include "mkff/abi.h"
#include "mkff/cpu_planes.h"
#include "mkff/error.h"
#include "mkff/linux/dmabuf.h"
#include "mkff/linux/drm.h"
#include "mkff/linux/va.h"
#include "mkff/log.h"
#include "mkff/macos/iosurface.h"
#include "mkff/types.h"
#include "mkff/video_decoder.h"
#include "mkff/video_frame.h"
#include "mkff/windows/d3d11.h"

MKFF_BEGIN_DECLS

/* Opaque platform-module-owned state, one instance per MKFF_Context. */
typedef struct MKFF_PlatformContext MKFF_PlatformContext;

typedef struct MKFF_PlatformAPI {
    MKFF_STRUCT_HEADER;

    MKFF_PlatformContext *(*context_create)(MKFF_LogCallback log_cb, void *log_user_data, MKFF_LogLevel min_level);
    void                  (*context_destroy)(MKFF_PlatformContext *pctx);
    const char           *(*context_get_last_error)(MKFF_PlatformContext *pctx);

    /*
     * Every field below this point is platform-specific and NULLABLE:
     * a given platform module only populates the handful of slots
     * matching its own OS, and leaves every other platform's slots as
     * NULL. That's safe because the *public* wrapper functions that
     * would call them (mkff_linux_*, mkff_macos_*, mkff_windows_*) are
     * themselves only compiled into libmkff for the matching
     * CMAKE_SYSTEM_NAME — an app built for Windows never links a call
     * path that could reach a NULL video_frame_export_dmabuf, etc.
     */

    /* --- Linux (VA-API / DRM / dma-buf) --- */
    MKFF_Result (*enumerate_drm_devices)(MKFF_PlatformContext *pctx,
                                          MKFF_DrmDeviceInfo *out_array,
                                          uint32_t array_capacity,
                                          uint32_t *out_count);

    MKFF_Result (*query_va_info)(MKFF_PlatformContext *pctx,
                                  const char *drm_device_path,
                                  MKFF_VaInfo *out_info);

    MKFF_Result (*query_va_profiles)(MKFF_PlatformContext *pctx,
                                      const char *drm_device_path,
                                      MKFF_VaProfileInfo *out_array,
                                      uint32_t array_capacity,
                                      uint32_t *out_count);

    MKFF_Result (*video_decoder_create)(MKFF_PlatformContext *pctx,
                                         const MKFF_VideoDecoderDesc *desc,
                                         void **out_decoder);
    void        (*video_decoder_destroy)(void *decoder);
    MKFF_Result (*video_decoder_submit)(void *decoder,
                                         const uint8_t *annex_b_data,
                                         size_t annex_b_size,
                                         int64_t pts,
                                         int64_t dts);
    MKFF_Result (*video_decoder_receive)(void *decoder, MKFF_VideoFrame **out_frame);
    MKFF_Result (*video_decoder_flush)(void *decoder);
    MKFF_Result (*video_decoder_get_info)(const void *decoder, MKFF_VideoDecoderInfo *out_info);

    MKFF_VideoFrame *(*video_frame_retain)(MKFF_VideoFrame *frame);
    void             (*video_frame_release)(MKFF_VideoFrame *frame);
    MKFF_Result      (*video_frame_get_info)(const MKFF_VideoFrame *frame, MKFF_VideoFrameInfo *out_info);

    /* --- Linux --- */
    MKFF_Result (*video_frame_export_dmabuf)(const MKFF_VideoFrame *frame, MKFF_LinuxDmaBufDesc *out_desc);
    /* --- macOS --- */
    MKFF_Result (*video_frame_export_iosurface)(const MKFF_VideoFrame *frame, MKFF_MacosIOSurfaceDesc *out_desc);
    /* --- Windows --- */
    MKFF_Result (*video_frame_export_d3d11_texture)(const MKFF_VideoFrame *frame, MKFF_WindowsD3D11TextureDesc *out_desc);

    /* Appended nullable CPU-plane map slots. Core only calls these when
     * platform struct_size covers them. Hardware backends may implement
     * GPU→CPU readback here, or leave NULL / return NOT_SUPPORTED. */
    MKFF_Result (*video_frame_map_cpu_planes)(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *out_planes);
    void        (*video_frame_unmap_cpu_planes)(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *planes);
} MKFF_PlatformAPI;

typedef const MKFF_PlatformAPI *(*MKFF_PlatformGetApiFn)(uint32_t requested_abi_version);

#define MKFF_PLATFORM_GET_API_SYMBOL "mkff_platform_get_api"

MKFF_END_DECLS

#endif /* MKFF_PLATFORM_H */
