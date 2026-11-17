#include <stdlib.h>
#include <string.h>

#include "decoder.h"
#include "mkff/platform.h"
#include "platform_api_table.h"
#include "platform_context.h"
#include "video_frame.h"

static MKFF_PlatformContext *pctx_create(MKFF_LogCallback log_cb, void *log_user_data, MKFF_LogLevel min_level) {
    MKFF_PlatformContext *pctx = (MKFF_PlatformContext *)calloc(1, sizeof(MKFF_PlatformContext));
    if (!pctx) return NULL;
    pctx->log_callback = log_cb;
    pctx->log_user_data = log_user_data;
    pctx->log_min_level = min_level;
    return pctx;
}

static void pctx_destroy(MKFF_PlatformContext *pctx) {
    free(pctx);
}

static const char *pctx_get_last_error(MKFF_PlatformContext *pctx) {
    return pctx ? pctx->last_error : "";
}

static const MKFF_PlatformAPI kMacosPlatformAPI = {
    .struct_size = sizeof(MKFF_PlatformAPI),
    .abi_version = MKFF_PLATFORM_ABI_VERSION,
    .reserved = {0, 0, 0, 0},

    .context_create = pctx_create,
    .context_destroy = pctx_destroy,
    .context_get_last_error = pctx_get_last_error,

    /* Linux (VA-API/DRM/dma-buf) and Windows (D3D11) concepts don't
     * apply here; the wrapper functions that would call these aren't
     * even compiled into libmkff on macOS (see src/core/CMakeLists.txt),
     * so these slots are unreachable. */
    .enumerate_drm_devices = NULL,
    .query_va_info = NULL,
    .query_va_profiles = NULL,

    .video_decoder_create = macos_video_decoder_create,
    .video_decoder_destroy = macos_video_decoder_destroy,
    .video_decoder_submit = macos_video_decoder_submit,
    .video_decoder_receive = macos_video_decoder_receive,
    .video_decoder_flush = macos_video_decoder_flush,
    .video_decoder_get_info = macos_video_decoder_get_info,

    .video_frame_retain = macos_video_frame_retain,
    .video_frame_release = macos_video_frame_release,
    .video_frame_get_info = macos_video_frame_get_info,

    .video_frame_export_dmabuf = NULL,     /* Linux-only */
    .video_frame_export_iosurface = macos_video_frame_export_iosurface,
    .video_frame_export_d3d11_texture = NULL, /* Windows-only */

    .video_frame_map_cpu_planes = macos_video_frame_map_cpu_planes,
    .video_frame_unmap_cpu_planes = macos_video_frame_unmap_cpu_planes,
};

const MKFF_PlatformAPI *mkff_macos_platform_api(void) {
    return &kMacosPlatformAPI;
}

MKFF_API const MKFF_PlatformAPI *mkff_platform_get_api(uint32_t requested_abi_version) {
    if (requested_abi_version != MKFF_PLATFORM_ABI_VERSION) {
        return NULL;
    }
    return &kMacosPlatformAPI;
}
