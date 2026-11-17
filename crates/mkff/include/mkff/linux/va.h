#ifndef MKFF_LINUX_VA_H
#define MKFF_LINUX_VA_H

#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/context.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"

MKFF_BEGIN_DECLS

typedef struct MKFF_VaInfo {
    MKFF_STRUCT_HEADER;

    char     vendor_string[256];
    int32_t  major_version;
    int32_t  minor_version;
} MKFF_VaInfo;

typedef struct MKFF_VaProfileInfo {
    MKFF_STRUCT_HEADER;

    MKFF_VideoCodec      codec;
    MKFF_VideoProfile    profile;
    MKFF_VideoEntrypoint entrypoint;
} MKFF_VaProfileInfo;

/* Opens `drm_device_path` (or the first enumerated render node if NULL),
 * initializes VA-API via vaGetDisplayDRM, and reports the driver vendor
 * string plus version. */
MKFF_API MKFF_Result mkff_linux_query_va_info(MKFF_Context *context,
                                               const char *drm_device_path,
                                               MKFF_VaInfo *out_info);

/* Enumerates supported (codec, profile, entrypoint) combinations among
 * H.264, HEVC, VP9 and AV1. Pass out_array == NULL and array_capacity ==
 * 0 to just retrieve the count via *out_count. */
MKFF_API MKFF_Result mkff_linux_query_va_profiles(MKFF_Context *context,
                                                   const char *drm_device_path,
                                                   MKFF_VaProfileInfo *out_array,
                                                   uint32_t array_capacity,
                                                   uint32_t *out_count);

MKFF_END_DECLS

#endif /* MKFF_LINUX_VA_H */
