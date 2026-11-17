#ifndef MKFF_MACOS_DECODER_SHARED_H
#define MKFF_MACOS_DECODER_SHARED_H

#include <CoreMedia/CoreMedia.h>
#include <VideoToolbox/VideoToolbox.h>
#include <pthread.h>
#include <stdatomic.h>

#include "mkff/log.h"
#include "mkff/types.h"

/*
 * Owns everything with GPU/OS lifetime tied to a decode session.
 * Unlike VA-API's or D3D11's explicit, caller-sized surface pool,
 * VideoToolbox manages its own internal buffer pool and exposes no
 * public API to cap it directly — so `pool_capacity`/`live_count` here
 * are a *self-imposed* cap this module enforces on top (refusing new
 * decode submissions once too many MKFF_VideoFrame references are
 * outstanding), not a hardware-level bound VT itself honors. This is a
 * real behavioral difference from the Linux/Windows backends, not an
 * oversight: it's the shape of the platform API.
 *
 * Reference counted for the same reason as the other backends'
 * DecoderShared: MKFF_VideoFrame objects are allowed to outlive the
 * MKFF_VideoDecoder that produced them.
 */
typedef struct DecoderShared {
    atomic_int refcount;

    VTDecompressionSessionRef   session;
    CMVideoFormatDescriptionRef format_description;
    int                          initialized;

    uint32_t coded_width;
    uint32_t coded_height;
    uint32_t display_width;
    uint32_t display_height;

    pthread_mutex_t live_lock;
    uint32_t        pool_capacity;
    uint32_t        live_count;

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

/* Returns 0 and reserves one slot if under `pool_capacity`, -1 if the
 * self-imposed cap is already reached. */
int  decoder_shared_pool_checkout(DecoderShared *shared);
void decoder_shared_pool_release(DecoderShared *shared);

void decoder_shared_set_error(DecoderShared *shared, const char *msg);

#endif /* MKFF_MACOS_DECODER_SHARED_H */
