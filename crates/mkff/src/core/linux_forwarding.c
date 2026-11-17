#include "mkff/linux/dmabuf.h"
#include "mkff/linux/drm.h"
#include "mkff/linux/va.h"

#include <string.h>
#include <unistd.h>

#include "context_internal.h"
#include "src/common/mkff_common.h"

MKFF_Result mkff_linux_enumerate_drm_devices(MKFF_Context *context,
                                              MKFF_DrmDeviceInfo *out_array,
                                              uint32_t array_capacity,
                                              uint32_t *out_count) {
    if (!context || !out_count) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    const MKFF_PlatformAPI *api = mkff_context_internal_get_platform_api(context);
    if (!api) {
        return MKFF_RESULT_ERROR_PLATFORM_LOAD;
    }
    return api->enumerate_drm_devices(mkff_context_internal_get_platform_context(context), out_array, array_capacity, out_count);
}

MKFF_Result mkff_linux_query_va_info(MKFF_Context *context, const char *drm_device_path, MKFF_VaInfo *out_info) {
    if (!context || !out_info) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    const MKFF_PlatformAPI *api = mkff_context_internal_get_platform_api(context);
    if (!api) {
        return MKFF_RESULT_ERROR_PLATFORM_LOAD;
    }
    return api->query_va_info(mkff_context_internal_get_platform_context(context), drm_device_path, out_info);
}

MKFF_Result mkff_linux_query_va_profiles(MKFF_Context *context,
                                          const char *drm_device_path,
                                          MKFF_VaProfileInfo *out_array,
                                          uint32_t array_capacity,
                                          uint32_t *out_count) {
    if (!context || !out_count) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    const MKFF_PlatformAPI *api = mkff_context_internal_get_platform_api(context);
    if (!api) {
        return MKFF_RESULT_ERROR_PLATFORM_LOAD;
    }
    return api->query_va_profiles(mkff_context_internal_get_platform_context(context), drm_device_path, out_array, array_capacity, out_count);
}

MKFF_Result mkff_linux_video_frame_export_dmabuf(const MKFF_VideoFrame *frame, MKFF_LinuxDmaBufDesc *out_desc) {
    if (!frame || !out_desc) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    const MKFF_HandleCommon *common = (const MKFF_HandleCommon *)frame;
    return common->api->video_frame_export_dmabuf(frame, out_desc);
}

void mkff_linux_dmabuf_desc_close(MKFF_LinuxDmaBufDesc *desc) {
    if (!desc) return;
    for (uint32_t i = 0; i < desc->num_objects && i < MKFF_LINUX_DMABUF_MAX_OBJECTS; i++) {
        if (desc->objects[i].fd >= 0) {
            close(desc->objects[i].fd);
        }
    }
    memset(desc, 0, sizeof(*desc));
}
