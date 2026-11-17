#include "decoder_shared.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "src/common/mkff_log_util.h"

DecoderShared *decoder_shared_create(uint32_t pool_capacity, MKFF_LogCallback log_cb, void *log_user_data, MKFF_LogLevel min_level) {
    DecoderShared *shared = (DecoderShared *)calloc(1, sizeof(DecoderShared));
    if (!shared) return NULL;

    atomic_init(&shared->refcount, 1);
    shared->pool_capacity = pool_capacity;
    shared->log_callback = log_cb;
    shared->log_user_data = log_user_data;
    shared->log_min_level = min_level;
    pthread_mutex_init(&shared->live_lock, NULL);
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

    if (shared->session) {
        VTDecompressionSessionInvalidate(shared->session);
        CFRelease(shared->session);
    }
    if (shared->format_description) {
        CFRelease(shared->format_description);
    }

    pthread_mutex_destroy(&shared->live_lock);
    free(shared);
}

int decoder_shared_pool_checkout(DecoderShared *shared) {
    int result = -1;
    pthread_mutex_lock(&shared->live_lock);
    if (shared->live_count < shared->pool_capacity) {
        shared->live_count++;
        result = 0;
    }
    pthread_mutex_unlock(&shared->live_lock);
    return result;
}

void decoder_shared_pool_release(DecoderShared *shared) {
    pthread_mutex_lock(&shared->live_lock);
    if (shared->live_count > 0) {
        shared->live_count--;
    }
    pthread_mutex_unlock(&shared->live_lock);
}

void decoder_shared_set_error(DecoderShared *shared, const char *msg) {
    if (!shared || !msg) return;
    size_t n = strlen(msg);
    if (n >= sizeof(shared->last_error)) {
        n = sizeof(shared->last_error) - 1;
    }
    memcpy(shared->last_error, msg, n);
    shared->last_error[n] = '\0';
    MKFF_LOG(shared->log_callback, shared->log_user_data, shared->log_min_level, MKFF_LOG_LEVEL_ERROR, "mkff.platform.macos", "%s", msg);
}
