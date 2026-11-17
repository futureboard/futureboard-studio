#ifndef MKFF_LINUX_DRM_H
#define MKFF_LINUX_DRM_H

#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/context.h"
#include "mkff/error.h"
#include "mkff/export.h"

MKFF_BEGIN_DECLS

typedef struct MKFF_DrmDeviceInfo {
    MKFF_STRUCT_HEADER;

    char     path[128];        /* e.g. "/dev/dri/renderD128" */
    char     driver_name[64];  /* e.g. "i915", "amdgpu", "nouveau" */
    uint32_t vendor_id;        /* PCI vendor id, 0 if unavailable */
    uint32_t device_id;        /* PCI device id, 0 if unavailable */
} MKFF_DrmDeviceInfo;

/* Enumerates /dev/dri/renderD* render nodes. Pass out_array == NULL and
 * array_capacity == 0 to just retrieve the count via *out_count. */
MKFF_API MKFF_Result mkff_linux_enumerate_drm_devices(MKFF_Context *context,
                                                        MKFF_DrmDeviceInfo *out_array,
                                                        uint32_t array_capacity,
                                                        uint32_t *out_count);

MKFF_END_DECLS

#endif /* MKFF_LINUX_DRM_H */
