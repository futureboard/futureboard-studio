#include "mkff/video_frame.h"
#include "mkff/cpu_planes.h"

#include <stddef.h>

#include "src/common/mkff_common.h"

static const MKFF_HandleCommon *as_common(const void *handle) {
    return (const MKFF_HandleCommon *)handle;
}

static int platform_has_cpu_plane_slots(const MKFF_PlatformAPI *api) {
    if (!api) {
        return 0;
    }
    const size_t needed = offsetof(MKFF_PlatformAPI, video_frame_unmap_cpu_planes)
                          + sizeof(api->video_frame_unmap_cpu_planes);
    return api->struct_size >= (uint32_t)needed;
}

MKFF_VideoFrame *mkff_video_frame_retain(MKFF_VideoFrame *frame) {
    if (!frame) return NULL;
    return as_common(frame)->api->video_frame_retain(frame);
}

void mkff_video_frame_release(MKFF_VideoFrame *frame) {
    if (!frame) return;
    as_common(frame)->api->video_frame_release(frame);
}

MKFF_Result mkff_video_frame_get_info(const MKFF_VideoFrame *frame, MKFF_VideoFrameInfo *out_info) {
    if (!frame || !out_info) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    return as_common(frame)->api->video_frame_get_info(frame, out_info);
}

MKFF_Result mkff_video_frame_map_cpu_planes(const MKFF_VideoFrame *frame,
                                             MKFF_CpuPlaneDesc *out_planes) {
    if (!frame || !out_planes) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    const MKFF_PlatformAPI *api = as_common(frame)->api;
    if (!platform_has_cpu_plane_slots(api) || !api->video_frame_map_cpu_planes) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }
    return api->video_frame_map_cpu_planes(frame, out_planes);
}

void mkff_video_frame_unmap_cpu_planes(const MKFF_VideoFrame *frame,
                                       MKFF_CpuPlaneDesc *planes) {
    if (!frame || !planes) {
        return;
    }
    const MKFF_PlatformAPI *api = as_common(frame)->api;
    if (!platform_has_cpu_plane_slots(api) || !api->video_frame_unmap_cpu_planes) {
        return;
    }
    api->video_frame_unmap_cpu_planes(frame, planes);
}
