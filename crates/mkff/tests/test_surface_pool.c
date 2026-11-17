#include <assert.h>
#include <stdio.h>

#include "src/platform/linux/decoder_shared.h"

/*
 * Exercises the bounded surface-pool checkout/release logic in
 * isolation from any real VA-API/DRM device: `decoder_shared_unref()`
 * only touches VA/DRM handles when `va_initialized` is set, which we
 * deliberately leave false here, so this runs anywhere (including CI
 * without a GPU) while still covering the real pool bookkeeping code
 * used by the decoder.
 */
int main(void) {
    const uint32_t capacity = 4;
    DecoderShared *shared = decoder_shared_create(capacity, NULL, NULL, MKFF_LOG_LEVEL_NONE);
    assert(shared != NULL);
    assert(shared->va_initialized == 0);

    uint32_t indices[4];
    VASurfaceID surfaces[4];
    for (uint32_t i = 0; i < capacity; i++) {
        int rc = decoder_shared_pool_checkout(shared, &indices[i], &surfaces[i]);
        assert(rc == 0);
    }

    /* Pool is now fully checked out: bounded, so the next checkout must
     * fail rather than silently overrun. */
    uint32_t extra_index;
    VASurfaceID extra_surface;
    int rc = decoder_shared_pool_checkout(shared, &extra_index, &extra_surface);
    assert(rc == -1);

    /* Releasing one slot makes exactly one more checkout available. */
    decoder_shared_pool_release(shared, indices[0]);
    rc = decoder_shared_pool_checkout(shared, &extra_index, &extra_surface);
    assert(rc == 0);
    assert(extra_index == indices[0]);

    /* Duplicate release must not corrupt bookkeeping / double-free a slot. */
    decoder_shared_pool_release(shared, indices[1]);
    decoder_shared_pool_release(shared, indices[1]);
    uint32_t reused_index;
    VASurfaceID reused_surface;
    rc = decoder_shared_pool_checkout(shared, &reused_index, &reused_surface);
    assert(rc == 0);
    assert(reused_index == indices[1]);

    decoder_shared_unref(shared); /* va_initialized == 0: no real VA/DRM calls made */

    printf("test_surface_pool: OK\n");
    return 0;
}
