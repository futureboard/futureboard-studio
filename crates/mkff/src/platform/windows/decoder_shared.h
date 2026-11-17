#ifndef MKFF_WINDOWS_DECODER_SHARED_H
#define MKFF_WINDOWS_DECODER_SHARED_H

#include <stdatomic.h>

#include "mkff/log.h"
#include "mkff/types.h"
#include "win_common.h"

/*
 * Owns everything with GPU/OS lifetime tied to a decode session: the
 * D3D11 device/decoder and the bounded surface pool (one texture
 * array, one ID3D11VideoDecoderOutputView per array slice). Reference
 * counted for the same reason as Linux's DecoderShared: MKFF_VideoFrame
 * objects are allowed to outlive the MKFF_VideoDecoder that produced
 * them, so the underlying D3D11 objects are only released once the
 * last reference (decoder + every live frame) drops.
 */
typedef struct DecoderShared {
    atomic_int refcount;

    ID3D11Device            *device;
    ID3D11DeviceContext     *device_context;
    ID3D11VideoDevice       *video_device;
    ID3D11VideoContext      *video_context;
    ID3D11VideoDecoder      *decoder;
    ID3D11Texture2D         *texture_array; /* one texture, ArraySize == pool_capacity */
    int                       initialized;

    uint32_t coded_width;
    uint32_t coded_height;
    uint32_t display_width;
    uint32_t display_height;
    DXGI_FORMAT dxgi_format; /* NV12 (Main 8-bit) or P010 (Main10) */
    MKFF_VideoProfile profile;

    CRITICAL_SECTION pool_lock;
    uint32_t pool_capacity;
    ID3D11VideoDecoderOutputView **output_views; /* one per array slice */
    int                           *pool_in_use;

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

/* Finds a free pool slot, marks it in use, and returns its array-slice
 * index via *out_index. Returns 0 on success, -1 if every slot is
 * currently checked out. */
int  decoder_shared_pool_checkout(DecoderShared *shared, uint32_t *out_index);
void decoder_shared_pool_release(DecoderShared *shared, uint32_t index);

void decoder_shared_set_error(DecoderShared *shared, const char *msg);

#endif /* MKFF_WINDOWS_DECODER_SHARED_H */
