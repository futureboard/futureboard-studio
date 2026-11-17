#ifndef MKFF_LINUX_VA_DISPLAY_H
#define MKFF_LINUX_VA_DISPLAY_H

#include <va/va.h>

#include "mkff/error.h"
#include "mkff/linux/va.h"

/* vaGetDisplayDRM() + vaInitialize(). Returns NULL on failure. */
VADisplay linux_va_open(int drm_fd, char *err_buf, size_t err_buf_size);
void      linux_va_close(VADisplay dpy);

MKFF_Result linux_va_get_info(VADisplay dpy, MKFF_VaInfo *out_info);

MKFF_Result linux_va_query_profiles(VADisplay dpy, MKFF_VaProfileInfo *out_array, uint32_t array_capacity, uint32_t *out_count);

/* Maps an H.264 profile_idc to a preference-ordered list of VAProfile
 * candidates to probe (some drivers only expose Main/High even for
 * Baseline content). Returns the number of candidates written. */
int linux_va_h264_profile_candidates(uint32_t profile_idc, VAProfile *out_candidates, int capacity);

/* Maps HEVC bit depth (8 → Main, 10 → Main10) to VAProfile candidates. */
int linux_va_hevc_profile_candidates(uint32_t bit_depth, VAProfile *out_candidates, int capacity);

/* Finds the first candidate profile that the display both reports via
 * vaQueryConfigProfiles() and supports VAEntrypointVLD for. Returns
 * VAProfileNone if none match. */
VAProfile linux_va_select_supported_profile(VADisplay dpy, const VAProfile *candidates, int count);

#endif /* MKFF_LINUX_VA_DISPLAY_H */
