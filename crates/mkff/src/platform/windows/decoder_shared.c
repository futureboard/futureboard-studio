#include "decoder_shared.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unknwn.h>

#include "src/common/mkff_log_util.h"

DecoderShared *decoder_shared_create(uint32_t pool_capacity, MKFF_LogCallback log_cb, void *log_user_data, MKFF_LogLevel min_level) {
    DecoderShared *shared = (DecoderShared *)calloc(1, sizeof(DecoderShared));
    if (!shared) return NULL;

    shared->output_views = (ID3D11VideoDecoderOutputView **)calloc(pool_capacity, sizeof(ID3D11VideoDecoderOutputView *));
    shared->pool_in_use = (int *)calloc(pool_capacity, sizeof(int));
    if (!shared->output_views || !shared->pool_in_use) {
        free(shared->output_views);
        free(shared->pool_in_use);
        free(shared);
        return NULL;
    }

    atomic_init(&shared->refcount, 1);
    shared->pool_capacity = pool_capacity;
    shared->log_callback = log_cb;
    shared->log_user_data = log_user_data;
    shared->log_min_level = min_level;
    InitializeCriticalSection(&shared->pool_lock);
    return shared;
}

DecoderShared *decoder_shared_ref(DecoderShared *shared) {
    if (shared) {
        atomic_fetch_add_explicit(&shared->refcount, 1, memory_order_relaxed);
    }
    return shared;
}

void decoder_shared_unref(DecoderShared *shared) {
    if (!shared) return;
    if (atomic_fetch_sub_explicit(&shared->refcount, 1, memory_order_acq_rel) != 1) {
        return;
    }

    if (shared->initialized) {
        for (uint32_t i = 0; i < shared->pool_capacity; i++) {
            if (shared->output_views[i]) {
                IUnknown_Release((IUnknown *)shared->output_views[i]);
            }
        }
        if (shared->decoder) {
            IUnknown_Release((IUnknown *)shared->decoder);
        }
        if (shared->texture_array) {
            IUnknown_Release((IUnknown *)shared->texture_array);
        }
        if (shared->video_context) {
            IUnknown_Release((IUnknown *)shared->video_context);
        }
        if (shared->video_device) {
            IUnknown_Release((IUnknown *)shared->video_device);
        }
        if (shared->device_context) {
            IUnknown_Release((IUnknown *)shared->device_context);
        }
        if (shared->device) {
            IUnknown_Release((IUnknown *)shared->device);
        }
    }

    DeleteCriticalSection(&shared->pool_lock);
    free(shared->output_views);
    free(shared->pool_in_use);
    free(shared);
}

int decoder_shared_pool_checkout(DecoderShared *shared, uint32_t *out_index) {
    int result = -1;
    EnterCriticalSection(&shared->pool_lock);
    for (uint32_t i = 0; i < shared->pool_capacity; i++) {
        if (!shared->pool_in_use[i]) {
            shared->pool_in_use[i] = 1;
            *out_index = i;
            result = 0;
            break;
        }
    }
    LeaveCriticalSection(&shared->pool_lock);
    return result;
}

void decoder_shared_pool_release(DecoderShared *shared, uint32_t index) {
    EnterCriticalSection(&shared->pool_lock);
    if (index < shared->pool_capacity) {
        shared->pool_in_use[index] = 0;
    }
    LeaveCriticalSection(&shared->pool_lock);
}

void decoder_shared_set_error(DecoderShared *shared, const char *msg) {
    if (!shared || !msg) return;
    size_t n = strlen(msg);
    if (n >= sizeof(shared->last_error)) {
        n = sizeof(shared->last_error) - 1;
    }
    memcpy(shared->last_error, msg, n);
    shared->last_error[n] = '\0';
    MKFF_LOG(shared->log_callback, shared->log_user_data, shared->log_min_level, MKFF_LOG_LEVEL_ERROR, "mkff.platform.windows", "%s", msg);
}
