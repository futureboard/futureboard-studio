#include <stdio.h>

#include "cli_common.h"

int cmd_va_info(int argc, char **argv) {
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

    MKFF_VaInfo info;
    MKFF_INIT_STRUCT_HEADER(&info);
    result = mkff_linux_query_va_info(ctx, NULL, &info);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "failed to query VA-API info: %s (%s)\n", mkff_result_to_string(result), mkff_context_get_last_error(ctx));
        mkff_context_destroy(ctx);
        return 1;
    }

    printf("VA-API vendor: %s\n", info.vendor_string);

    MKFF_VaProfileInfo profiles[64];
    uint32_t count = 0;
    result = mkff_linux_query_va_profiles(ctx, NULL, profiles, 64, &count);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "failed to query VA-API profiles: %s (%s)\n", mkff_result_to_string(result), mkff_context_get_last_error(ctx));
        mkff_context_destroy(ctx);
        return 1;
    }

    printf("supported decode profiles (VLD entrypoint):\n");
    if (count == 0) {
        printf("  (none)\n");
    }
    for (uint32_t i = 0; i < count && i < 64; i++) {
        printf("  %-8s %s\n", cli_codec_name(profiles[i].codec), cli_profile_name(profiles[i].profile));
    }

    mkff_context_destroy(ctx);
    return 0;
}
