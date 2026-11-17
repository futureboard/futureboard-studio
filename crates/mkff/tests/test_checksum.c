#include <assert.h>
#include <stdio.h>
#include <stdlib.h>

#include "src/core/simd/checksum.h"

int main(void) {
    for (size_t len = 0; len < 300; len += 7) {
        uint8_t *buf = (uint8_t *)malloc(len > 0 ? len : 1);
        for (size_t i = 0; i < len; i++) {
            buf[i] = (uint8_t)((i * 31 + 17) & 0xFF);
        }

        uint32_t scalar = mkff_internal_buffer_checksum_scalar(buf, len);
        uint32_t dispatched = mkff_internal_buffer_checksum(buf, len);
        assert(scalar == dispatched);

        free(buf);
    }

    printf("test_checksum: OK\n");
    return 0;
}
