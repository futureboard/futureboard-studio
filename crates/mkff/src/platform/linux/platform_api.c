#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "decoder.h"
#include "drm_device.h"
#include "mkff/platform.h"
#include "platform_api_table.h"
#include "platform_context.h"
#include "va_display.h"
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

static void pctx_set_error(MKFF_PlatformContext *pctx, const char *msg) {
    if (!pctx || !msg) return;
    snprintf(pctx->last_error, sizeof(pctx->last_error), "%s", msg);
}

static MKFF_Result api_enumerate_drm_devices(MKFF_PlatformContext *pctx, MKFF_DrmDeviceInfo *out_array, uint32_t array_capacity, uint32_t *out_count) {
    char err[256] = {0};
    MKFF_Result result = linux_enumerate_drm_devices(out_array, array_capacity, out_count, err, sizeof(err));
    if (result != MKFF_RESULT_OK) {
        pctx_set_error(pctx, err);
    }
    return result;
}

static MKFF_Result api_query_va_info(MKFF_PlatformContext *pctx, const char *drm_device_path, MKFF_VaInfo *out_info) {
    char err[256] = {0};
    int fd = linux_open_drm_device(drm_device_path, err, sizeof(err));
    if (fd < 0) {
        pctx_set_error(pctx, err);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    VADisplay dpy = linux_va_open(fd, err, sizeof(err));
    if (!dpy) {
        close(fd);
        pctx_set_error(pctx, err);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    MKFF_Result result = linux_va_get_info(dpy, out_info);
    linux_va_close(dpy);
    close(fd);
    return result;
}

static MKFF_Result api_query_va_profiles(MKFF_PlatformContext *pctx,
                                          const char *drm_device_path,
                                          MKFF_VaProfileInfo *out_array,
                                          uint32_t array_capacity,
                                          uint32_t *out_count) {
    char err[256] = {0};
    int fd = linux_open_drm_device(drm_device_path, err, sizeof(err));
    if (fd < 0) {
        pctx_set_error(pctx, err);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    VADisplay dpy = linux_va_open(fd, err, sizeof(err));
    if (!dpy) {
        close(fd);
        pctx_set_error(pctx, err);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    MKFF_Result result = linux_va_query_profiles(dpy, out_array, array_capacity, out_count);
    linux_va_close(dpy);
    close(fd);
    return result;
}

static const MKFF_PlatformAPI kLinuxPlatformAPI = {
    .struct_size = sizeof(MKFF_PlatformAPI),
    .abi_version = MKFF_PLATFORM_ABI_VERSION,
    .reserved = {0, 0, 0, 0},

    .context_create = pctx_create,
    .context_destroy = pctx_destroy,
    .context_get_last_error = pctx_get_last_error,

    .enumerate_drm_devices = api_enumerate_drm_devices,
    .query_va_info = api_query_va_info,
    .query_va_profiles = api_query_va_profiles,

    .video_decoder_create = linux_video_decoder_create,
    .video_decoder_destroy = linux_video_decoder_destroy,
    .video_decoder_submit = linux_video_decoder_submit,
    .video_decoder_receive = linux_video_decoder_receive,
    .video_decoder_flush = linux_video_decoder_flush,
    .video_decoder_get_info = linux_video_decoder_get_info,

    .video_frame_retain = linux_video_frame_retain,
    .video_frame_release = linux_video_frame_release,
    .video_frame_get_info = linux_video_frame_get_info,
    .video_frame_export_dmabuf = linux_video_frame_export_dmabuf,
    .video_frame_export_iosurface = NULL,       /* macOS-only */
    .video_frame_export_d3d11_texture = NULL,   /* Windows-only */
    .video_frame_map_cpu_planes = linux_video_frame_map_cpu_planes,
    .video_frame_unmap_cpu_planes = linux_video_frame_unmap_cpu_planes,
};

const MKFF_PlatformAPI *mkff_linux_platform_api(void) {
    return &kLinuxPlatformAPI;
}

MKFF_API const MKFF_PlatformAPI *mkff_platform_get_api(uint32_t requested_abi_version) {
    if (requested_abi_version != MKFF_PLATFORM_ABI_VERSION) {
        return NULL;
    }
    return &kLinuxPlatformAPI;
}
