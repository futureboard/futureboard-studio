#include "drm_device.h"

#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <xf86drm.h>

#define LINUX_MAX_DRM_DEVICES 32

static void set_err(char *err_buf, size_t err_buf_size, const char *msg) {
    if (err_buf && err_buf_size > 0) {
        snprintf(err_buf, err_buf_size, "%s", msg);
    }
}

MKFF_Result linux_enumerate_drm_devices(MKFF_DrmDeviceInfo *out_array, uint32_t array_capacity, uint32_t *out_count, char *err_buf, size_t err_buf_size) {
    (void)err_buf;
    (void)err_buf_size;

    drmDevicePtr devices[LINUX_MAX_DRM_DEVICES];
    int count = drmGetDevices2(0, devices, LINUX_MAX_DRM_DEVICES);
    if (count < 0) {
        /* No /dev/dri (e.g. a container/CI host with no GPU passed
         * through) is a legitimate "found nothing" state, not a hard
         * failure: report zero devices rather than erroring. */
        *out_count = 0;
        return MKFF_RESULT_OK;
    }

    uint32_t written = 0;
    for (int i = 0; i < count; i++) {
        drmDevicePtr dev = devices[i];
        if (!(dev->available_nodes & (1 << DRM_NODE_RENDER))) {
            continue;
        }
        const char *node_path = dev->nodes[DRM_NODE_RENDER];
        if (!node_path) {
            continue;
        }

        if (out_array && written < array_capacity) {
            MKFF_DrmDeviceInfo *info = &out_array[written];
            memset(info, 0, sizeof(*info));
            MKFF_INIT_STRUCT_HEADER(info);
            snprintf(info->path, sizeof(info->path), "%s", node_path);

            int fd = open(node_path, O_RDWR | O_CLOEXEC);
            if (fd >= 0) {
                drmVersionPtr version = drmGetVersion(fd);
                if (version) {
                    snprintf(info->driver_name, sizeof(info->driver_name), "%.*s", version->name_len, version->name ? version->name : "");
                    drmFreeVersion(version);
                }
                close(fd);
            }

            if (dev->bustype == DRM_BUS_PCI && dev->deviceinfo.pci) {
                info->vendor_id = dev->deviceinfo.pci->vendor_id;
                info->device_id = dev->deviceinfo.pci->device_id;
            }
        }
        written++;
    }

    drmFreeDevices(devices, count);

    *out_count = written;
    return MKFF_RESULT_OK;
}

int linux_open_drm_device(const char *path, char *err_buf, size_t err_buf_size) {
    char resolved_path[128];
    const char *use_path = path;

    if (!use_path) {
        MKFF_DrmDeviceInfo info;
        uint32_t count = 0;
        if (linux_enumerate_drm_devices(&info, 1, &count, err_buf, err_buf_size) != MKFF_RESULT_OK || count == 0) {
            set_err(err_buf, err_buf_size, "no DRM render nodes found");
            return -1;
        }
        snprintf(resolved_path, sizeof(resolved_path), "%s", info.path);
        use_path = resolved_path;
    }

    int fd = open(use_path, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        set_err(err_buf, err_buf_size, "failed to open DRM render node");
        return -1;
    }
    return fd;
}
