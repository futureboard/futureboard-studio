#include "va_display.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <va/va_drm.h>

static void set_err(char *err_buf, size_t err_buf_size, const char *msg) {
    if (err_buf && err_buf_size > 0) {
        snprintf(err_buf, err_buf_size, "%s", msg);
    }
}

VADisplay linux_va_open(int drm_fd, char *err_buf, size_t err_buf_size) {
    VADisplay dpy = vaGetDisplayDRM(drm_fd);
    if (!dpy) {
        set_err(err_buf, err_buf_size, "vaGetDisplayDRM failed");
        return NULL;
    }

    int major = 0, minor = 0;
    VAStatus status = vaInitialize(dpy, &major, &minor);
    if (status != VA_STATUS_SUCCESS) {
        set_err(err_buf, err_buf_size, vaErrorStr(status));
        return NULL;
    }
    return dpy;
}

void linux_va_close(VADisplay dpy) {
    if (dpy) {
        vaTerminate(dpy);
    }
}

MKFF_Result linux_va_get_info(VADisplay dpy, MKFF_VaInfo *out_info) {
    uint32_t requested_size = out_info->struct_size;
    memset(out_info, 0, sizeof(*out_info));
    MKFF_INIT_STRUCT_HEADER(out_info);
    out_info->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_info);

    const char *vendor = vaQueryVendorString(dpy);
    if (vendor) {
        snprintf(out_info->vendor_string, sizeof(out_info->vendor_string), "%s", vendor);
    }
    /* libva does not expose a direct getter for the negotiated
     * major/minor after vaInitialize(); vaInitialize() itself returned
     * them, so decoder/CLI callers that need the number pass it along
     * separately. Vendor string alone is what downstream commonly cares
     * about (driver identification). */
    return MKFF_RESULT_OK;
}

typedef struct ProfileMap {
    VAProfile         va_profile;
    MKFF_VideoCodec   codec;
    MKFF_VideoProfile mkff_profile;
} ProfileMap;

static const ProfileMap kProfileMap[] = {
    {VAProfileH264ConstrainedBaseline, MKFF_VIDEO_CODEC_H264, MKFF_VIDEO_PROFILE_H264_CONSTRAINED_BASELINE},
    {VAProfileH264Main,                MKFF_VIDEO_CODEC_H264, MKFF_VIDEO_PROFILE_H264_MAIN},
    {VAProfileH264High,                MKFF_VIDEO_CODEC_H264, MKFF_VIDEO_PROFILE_H264_HIGH},
    {VAProfileHEVCMain,                MKFF_VIDEO_CODEC_HEVC, MKFF_VIDEO_PROFILE_HEVC_MAIN},
    {VAProfileHEVCMain10,              MKFF_VIDEO_CODEC_HEVC, MKFF_VIDEO_PROFILE_HEVC_MAIN10},
    {VAProfileVP9Profile0,             MKFF_VIDEO_CODEC_VP9,  MKFF_VIDEO_PROFILE_VP9_PROFILE0},
    {VAProfileVP9Profile2,             MKFF_VIDEO_CODEC_VP9,  MKFF_VIDEO_PROFILE_VP9_PROFILE2},
    {VAProfileAV1Profile0,             MKFF_VIDEO_CODEC_AV1,  MKFF_VIDEO_PROFILE_AV1_MAIN},
};
#define kProfileMapCount (sizeof(kProfileMap) / sizeof(kProfileMap[0]))

MKFF_Result linux_va_query_profiles(VADisplay dpy, MKFF_VaProfileInfo *out_array, uint32_t array_capacity, uint32_t *out_count) {
    int max_profiles = vaMaxNumProfiles(dpy);
    if (max_profiles <= 0) {
        *out_count = 0;
        return MKFF_RESULT_OK;
    }

    VAProfile *profiles = (VAProfile *)malloc((size_t)max_profiles * sizeof(VAProfile));
    if (!profiles) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    int num_profiles = 0;
    VAStatus status = vaQueryConfigProfiles(dpy, profiles, &num_profiles);
    if (status != VA_STATUS_SUCCESS) {
        free(profiles);
        *out_count = 0;
        return MKFF_RESULT_ERROR_DEVICE;
    }

    int max_entrypoints = vaMaxNumEntrypoints(dpy);
    VAEntrypoint *entrypoints = (VAEntrypoint *)malloc((size_t)(max_entrypoints > 0 ? max_entrypoints : 1) * sizeof(VAEntrypoint));
    if (!entrypoints) {
        free(profiles);
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    uint32_t written = 0;
    for (int p = 0; p < num_profiles; p++) {
        for (size_t m = 0; m < kProfileMapCount; m++) {
            if (kProfileMap[m].va_profile != profiles[p]) continue;

            int num_ep = 0;
            status = vaQueryConfigEntrypoints(dpy, profiles[p], entrypoints, &num_ep);
            if (status != VA_STATUS_SUCCESS) continue;

            int has_vld = 0;
            for (int e = 0; e < num_ep; e++) {
                if (entrypoints[e] == VAEntrypointVLD) {
                    has_vld = 1;
                    break;
                }
            }
            if (!has_vld) continue;

            if (out_array && written < array_capacity) {
                MKFF_VaProfileInfo *info = &out_array[written];
                memset(info, 0, sizeof(*info));
                MKFF_INIT_STRUCT_HEADER(info);
                info->codec = kProfileMap[m].codec;
                info->profile = kProfileMap[m].mkff_profile;
                info->entrypoint = MKFF_VIDEO_ENTRYPOINT_VLD;
            }
            written++;
        }
    }

    free(entrypoints);
    free(profiles);
    *out_count = written;
    return MKFF_RESULT_OK;
}

int linux_va_h264_profile_candidates(uint32_t profile_idc, VAProfile *out_candidates, int capacity) {
    int n = 0;
    switch (profile_idc) {
        case 66: /* Baseline / Constrained Baseline */
            if (n < capacity) out_candidates[n++] = VAProfileH264ConstrainedBaseline;
            if (n < capacity) out_candidates[n++] = VAProfileH264Main;
            break;
        case 77: /* Main */
            if (n < capacity) out_candidates[n++] = VAProfileH264Main;
            if (n < capacity) out_candidates[n++] = VAProfileH264High;
            break;
        case 100: /* High */
            if (n < capacity) out_candidates[n++] = VAProfileH264High;
            break;
        default:
            break;
    }
    return n;
}

int linux_va_hevc_profile_candidates(uint32_t bit_depth, VAProfile *out_candidates, int capacity) {
    int n = 0;
    if (bit_depth == 10) {
        if (n < capacity) out_candidates[n++] = VAProfileHEVCMain10;
    } else if (bit_depth == 8) {
        if (n < capacity) out_candidates[n++] = VAProfileHEVCMain;
    }
    return n;
}

VAProfile linux_va_select_supported_profile(VADisplay dpy, const VAProfile *candidates, int count) {
    int max_profiles = vaMaxNumProfiles(dpy);
    if (max_profiles <= 0) return VAProfileNone;

    VAProfile *profiles = (VAProfile *)malloc((size_t)max_profiles * sizeof(VAProfile));
    if (!profiles) return VAProfileNone;
    int num_profiles = 0;
    if (vaQueryConfigProfiles(dpy, profiles, &num_profiles) != VA_STATUS_SUCCESS) {
        free(profiles);
        return VAProfileNone;
    }

    int max_entrypoints = vaMaxNumEntrypoints(dpy);
    VAEntrypoint *entrypoints = (VAEntrypoint *)malloc((size_t)(max_entrypoints > 0 ? max_entrypoints : 1) * sizeof(VAEntrypoint));
    if (!entrypoints) {
        free(profiles);
        return VAProfileNone;
    }

    VAProfile selected = VAProfileNone;
    for (int c = 0; c < count && selected == VAProfileNone; c++) {
        for (int p = 0; p < num_profiles; p++) {
            if (profiles[p] != candidates[c]) continue;
            int num_ep = 0;
            if (vaQueryConfigEntrypoints(dpy, profiles[p], entrypoints, &num_ep) != VA_STATUS_SUCCESS) continue;
            for (int e = 0; e < num_ep; e++) {
                if (entrypoints[e] == VAEntrypointVLD) {
                    selected = candidates[c];
                    break;
                }
            }
            break;
        }
    }

    free(entrypoints);
    free(profiles);
    return selected;
}
