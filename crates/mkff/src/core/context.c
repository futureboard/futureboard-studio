#include "mkff/context.h"

#include <stdlib.h>
#include <string.h>

#include "context_internal.h"
#include "platform_loader.h"
#include "src/common/mkff_log_util.h"

struct MKFF_Context {
    MKFF_LogCallback log_callback;
    void            *log_user_data;
    MKFF_LogLevel    log_min_level;

    MKFF_PlatformModule platform_module;
    MKFF_PlatformContext *platform_context; /* may be NULL if the platform module failed to load */

    char last_error[512];
};

static void set_last_error(MKFF_Context *ctx, const char *msg) {
    if (!ctx) return;
    size_t n = strlen(msg);
    if (n >= sizeof(ctx->last_error)) {
        n = sizeof(ctx->last_error) - 1;
    }
    memcpy(ctx->last_error, msg, n);
    ctx->last_error[n] = '\0';
}

MKFF_Result mkff_context_create(const MKFF_ContextDesc *desc, MKFF_Context **out_context) {
    if (!out_context) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    *out_context = NULL;

    MKFF_ContextDesc local_desc;
    if (desc) {
        if (desc->struct_size < sizeof(uint32_t) * 2) {
            return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
        }
        local_desc = *desc;
    } else {
        memset(&local_desc, 0, sizeof(local_desc));
        MKFF_INIT_STRUCT_HEADER(&local_desc);
        local_desc.log_min_level = MKFF_LOG_LEVEL_INFO;
    }

    MKFF_Context *ctx = (MKFF_Context *)calloc(1, sizeof(MKFF_Context));
    if (!ctx) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    ctx->log_callback  = local_desc.log_callback;
    ctx->log_user_data = local_desc.log_user_data;
    ctx->log_min_level = local_desc.log_min_level;
    set_last_error(ctx, "");

    MKFF_Result load_result = mkff_platform_module_load(local_desc.platform_module_path, &ctx->platform_module);
    if (load_result != MKFF_RESULT_OK) {
        MKFF_LOG(ctx->log_callback, ctx->log_user_data, ctx->log_min_level, MKFF_LOG_LEVEL_WARN,
                 "mkff.context",
                 "platform module unavailable (%s); DRM/VA-API/decoder functionality will report "
                 "MKFF_RESULT_ERROR_PLATFORM_LOAD",
                 mkff_result_to_string(load_result));
        set_last_error(ctx, "platform module could not be loaded");
        /* Context creation still succeeds: a context with no platform
         * module can still be used for API introspection and error
         * reporting, matching "loader failure is reported per-call". */
        *out_context = ctx;
        return MKFF_RESULT_OK;
    }

    ctx->platform_context = ctx->platform_module.api->context_create(ctx->log_callback, ctx->log_user_data, ctx->log_min_level);
    if (!ctx->platform_context) {
        MKFF_LOG(ctx->log_callback, ctx->log_user_data, ctx->log_min_level, MKFF_LOG_LEVEL_ERROR,
                 "mkff.context", "%s", "platform module loaded but its context_create() failed");
        set_last_error(ctx, "platform context_create failed");
        mkff_platform_module_unload(&ctx->platform_module);
    }

    *out_context = ctx;
    return MKFF_RESULT_OK;
}

void mkff_context_destroy(MKFF_Context *context) {
    if (!context) return;
    if (context->platform_context) {
        context->platform_module.api->context_destroy(context->platform_context);
    }
    mkff_platform_module_unload(&context->platform_module);
    free(context);
}

const char *mkff_context_get_last_error(MKFF_Context *context) {
    if (!context) return "";
    if (context->platform_context && context->platform_module.api->context_get_last_error) {
        const char *platform_err = context->platform_module.api->context_get_last_error(context->platform_context);
        if (platform_err && platform_err[0] != '\0') {
            return platform_err;
        }
    }
    return context->last_error;
}

/* --- internal accessors used by other core translation units --- */

const MKFF_PlatformAPI *mkff_context_internal_get_platform_api(MKFF_Context *context) {
    return context && context->platform_context ? context->platform_module.api : NULL;
}

MKFF_PlatformContext *mkff_context_internal_get_platform_context(MKFF_Context *context) {
    return context ? context->platform_context : NULL;
}
