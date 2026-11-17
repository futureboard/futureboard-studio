#include "decoder_shared.h"

#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "src/common/mkff_log_util.h"

DecoderShared *decoder_shared_create(uint32_t pool_capacity, MKFF_LogCallback log_cb, void *log_user_data, MKFF_LogLevel min_level) {
    DecoderShared *shared = (DecoderShared *)calloc(1, sizeof(DecoderShared));
    if (!shared) return NULL;

    shared->pool_surfaces = (VASurfaceID *)calloc(pool_capacity, sizeof(VASurfaceID));
    shared->pool_in_use = (int *)calloc(pool_capacity, sizeof(int));
    if (!shared->pool_surfaces || !shared->pool_in_use) {
        free(shared->pool_surfaces);
        free(shared->pool_in_use);
        free(shared);
        return NULL;
    }

    atomic_init(&shared->refcount, 1);
    shared->drm_fd = -1;
    shared->pool_capacity = pool_capacity;
    shared->log_callback = log_cb;
    shared->log_user_data = log_user_data;
    shared->log_min_level = min_level;
    pthread_mutex_init(&shared->pool_lock, NULL);
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

    if (shared->va_context_created) {
        vaDestroyContext(shared->va_dpy, shared->va_context);
    }
    if (shared->va_config != VA_INVALID_ID && shared->va_initialized) {
        vaDestroyConfig(shared->va_dpy, shared->va_config);
    }
    if (shared->pool_capacity > 0 && shared->va_initialized) {
        vaDestroySurfaces(shared->va_dpy, shared->pool_surfaces, (int)shared->pool_capacity);
    }
    if (shared->va_initialized) {
        vaTerminate(shared->va_dpy);
    }
    if (shared->drm_fd >= 0) {
        close(shared->drm_fd);
    }

    pthread_mutex_destroy(&shared->pool_lock);
    free(shared->pool_surfaces);
    free(shared->pool_in_use);
    free(shared);
}

int decoder_shared_pool_checkout(DecoderShared *shared, uint32_t *out_index, VASurfaceID *out_surface) {
    int result = -1;
    pthread_mutex_lock(&shared->pool_lock);
    for (uint32_t i = 0; i < shared->pool_capacity; i++) {
        if (!shared->pool_in_use[i]) {
            shared->pool_in_use[i] = 1;
            *out_index = i;
            *out_surface = shared->pool_surfaces[i];
            result = 0;
            break;
        }
    }
    pthread_mutex_unlock(&shared->pool_lock);
    return result;
}

void decoder_shared_pool_release(DecoderShared *shared, uint32_t index) {
    pthread_mutex_lock(&shared->pool_lock);
    if (index < shared->pool_capacity) {
        shared->pool_in_use[index] = 0;
    }
    pthread_mutex_unlock(&shared->pool_lock);
}

void decoder_shared_set_error(DecoderShared *shared, const char *msg) {
    if (!shared || !msg) return;
    size_t n = strlen(msg);
    if (n >= sizeof(shared->last_error)) {
        n = sizeof(shared->last_error) - 1;
    }
    memcpy(shared->last_error, msg, n);
    shared->last_error[n] = '\0';
    MKFF_LOG(shared->log_callback, shared->log_user_data, shared->log_min_level, MKFF_LOG_LEVEL_ERROR, "mkff.platform.linux", "%s", msg);
}
