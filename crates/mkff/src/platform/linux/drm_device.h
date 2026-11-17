#ifndef MKFF_LINUX_DRM_DEVICE_H
#define MKFF_LINUX_DRM_DEVICE_H

#include <stddef.h>

#include "mkff/error.h"
#include "mkff/linux/drm.h"

MKFF_Result linux_enumerate_drm_devices(MKFF_DrmDeviceInfo *out_array, uint32_t array_capacity, uint32_t *out_count, char *err_buf, size_t err_buf_size);

/* Opens a render node by path (or the first enumerated one if path ==
 * NULL). Returns the open fd, or -1 on failure. */
int linux_open_drm_device(const char *path, char *err_buf, size_t err_buf_size);

#endif /* MKFF_LINUX_DRM_DEVICE_H */
