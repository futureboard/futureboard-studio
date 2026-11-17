#include <stdio.h>

#include "cli_common.h"

int cmd_devices(int argc, char **argv) {
    (void)argc;
    (void)argv;

    MKFF_ContextDesc desc;
    MKFF_INIT_STRUCT_HEADER(&desc);
    desc.log_callback = cli_log_callback;
    desc.log_min_level = MKFF_LOG_LEVEL_WARN;

    MKFF_Context *ctx = NULL;
    MKFF_Result result = mkff_context_create(&desc, &ctx);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "mkff_context_create failed: %s\n", mkff_result_to_string(result));
        return 1;
    }

    MKFF_DrmDeviceInfo devices[16];
    uint32_t count = 0;
    result = mkff_linux_enumerate_drm_devices(ctx, devices, 16, &count);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "failed to enumerate DRM devices: %s (%s)\n", mkff_result_to_string(result), mkff_context_get_last_error(ctx));
        mkff_context_destroy(ctx);
        return 1;
    }

    if (count == 0) {
        printf("no DRM render nodes found\n");
    } else {
        printf("%-24s %-16s %-10s %-10s\n", "PATH", "DRIVER", "VENDOR", "DEVICE");
        for (uint32_t i = 0; i < count && i < 16; i++) {
            printf("%-24s %-16s 0x%08x 0x%08x\n", devices[i].path, devices[i].driver_name, devices[i].vendor_id, devices[i].device_id);
        }
    }

    mkff_context_destroy(ctx);
    return 0;
}
