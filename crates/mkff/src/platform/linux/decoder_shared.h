#ifndef MKFF_LINUX_DECODER_SHARED_H
#define MKFF_LINUX_DECODER_SHARED_H

#include <pthread.h>
#include <stdatomic.h>
#include <va/va.h>

#include "mkff/log.h"
#include "mkff/types.h"

/*
 * Owns everything with GPU/OS lifetime tied to a decode session: the VA
 * display/config/context and the bounded surface pool. Reference
 * counted because MKFF_VideoFrame objects (which wrap one pool surface
 * each) are allowed to outlive the MKFF_VideoDecoder that produced them
 * — the decoder holds one reference, and every live LinuxVideoFrame
 * holds one more. The underlying VA objects are torn down only once the
 * last reference drops.
 */
typedef struct DecoderShared {
    atomic_int refcount;

    int       drm_fd;
    VADisplay va_dpy;
    int       va_initialized;
    VAConfigID  va_config;
    VAContextID va_context;
    int       va_context_created;

    uint32_t coded_width;   /* picture_width_in_mbs * 16 */
    uint32_t coded_height;  /* picture_height_in_mbs * 16 */
    uint32_t display_width;
    uint32_t display_height;
    MKFF_VideoProfile profile;

    pthread_mutex_t pool_lock;
    uint32_t     pool_capacity;
    VASurfaceID *pool_surfaces;
    int         *pool_in_use;

    MKFF_LogCallback log_callback;
    void            *log_user_data;
    MKFF_LogLevel    log_min_level;

    char last_error[256];
} DecoderShared;

DecoderShared *decoder_shared_create(uint32_t pool_capacity,
                                      MKFF_LogCallback log_cb,
                                      void *log_user_data,
                                      MKFF_LogLevel min_level);
DecoderShared *decoder_shared_ref(DecoderShared *shared);
void           decoder_shared_unref(DecoderShared *shared);

/* Finds a free pool slot, marks it in use, and returns its index via
 * *out_index (and surface id via *out_surface). Returns 0 on success,
 * -1 if every slot is currently checked out. */
int  decoder_shared_pool_checkout(DecoderShared *shared, uint32_t *out_index, VASurfaceID *out_surface);
void decoder_shared_pool_release(DecoderShared *shared, uint32_t index);

void decoder_shared_set_error(DecoderShared *shared, const char *msg);

#endif /* MKFF_LINUX_DECODER_SHARED_H */
